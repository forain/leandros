# lane/uaccess — 2026-10-19

Bug list L5.1 (P0): shallow user-pointer validation, and user memory touched
while a server lock is held. A bad pointer killed the caller (an unresolvable
kernel-mode fault on a user address is a SIGSEGV kill since 0625d2d7), and a
caller killed while holding a lock wedged every later user of that lock.

The fault-tolerant helpers already existed (`sched::uaccess::copy_from_user`,
`copy_to_user`, `read_user`, `write_user`, 0625d2d7); the sites below just
did not use them.

## Per site

| site | was | now |
|---|---|---|
| rt_sigaction act/oldact, rt_sigprocmask set/oldset | raw `ptr::read/write` (outside RUN_QUEUE) | `read_user`/`write_user`, EFAULT |
| sigaltstack ss/oss (sibling) | raw, oss written before ss read | uaccess; ss read first (Linux order) |
| getdents64 | 1-byte check; VFS / f2fs filled the user buffer under FD_TABLES / TMP_FILES / the mount lock | kernel bounce buffer (<= 64 KiB), copied out with nothing held |
| getcwd | already `copy_to_user` of the whole result | ALREADY FIXED (the 1-byte check is only the early-out) |
| mount source/target/fstype | already `copy_user_str` | ALREADY FIXED |
| memfd_create name | raw byte loop | `copy_user_str` (249 + NUL, EINVAL beyond; NULL is EFAULT as on Linux); '/' in the name mapped to '_' |
| ioctl, pty (PTYS lock) and console (CONSOLE_TERMIOS) | range check + prefault, raw copy under the lock | `tty_ioctl_bounce`: per-command size + direction, copy in before / out after through uaccess, the handler sees a kernel buffer |
| ioctl, VT/KD | raw, no lock | uaccess (`put`/`get` in vt.rs) |
| ioctl, kernel-side (FIONREAD fd0, FIONBIO, socket FIONREAD/TIOCOUTQ, PRIME, EXECBUFFER fence fd) | raw after a range check | `read_user`/`write_user` |
| ioctl, VFS FIONREAD (RAM file stored with FD_TABLES held), FBIOGET_VSCREENINFO | raw / page-walk | lock dropped first, `write_user` |
| ioctl, block devices (BLK*, BLKPG, LOOP_GET_STATUS64) | raw | uaccess |
| pipewire ioctls (SET_PARAMS/GET_DELAY/SND_GET_INFO) | `read/write_user_buf` page walk with STATE held | copies outside STATE through uaccess; VFS_WRITE uses `copy_from_user` (the page walk also failed on not-yet-faulted lazy pages) |
| pipe2 (sibling) | both fds stored into the user array with FD_TABLES held | kernel array, `write_user` after; EFAULT closes both ends |
| {,l,f}{get,set,list,remove}xattr (sibling) | name/value/list touched by VFS/f2fs under their locks (prefault only) | name copied (ERANGE > 255), value/list bounced |

## Test

syscalltest gains `uaccess_signal`, `uaccess_fs`, `uaccess_pty` (each syscall
above against NULL, an unmapped page, a buffer straddling the end of a mapping,
and a kernel address, in a watchdogged child: must be EFAULT, child alive) and
`uaccess_survivor` (a second process then uses the pty, pipes, FIONREAD,
getdents, getcwd, sigaction, memfd).

## Left open

- `kernel/src/syncfile_ioctl.rs` still reads/writes its args raw (range check
  only; no lock held, so a bad pointer kills the caller but wedges nothing).
- DRM/evdev device servers: their own argument copies were not audited here.
- ~140 other raw user derefs in `kernel/src/syscall.rs` outside the listed
  syscalls (mostly no lock held: kill, not deadlock). Worth a mechanical sweep.
- getdents64 into a buffer that faults on copy-out has already advanced the
  directory cursor (the entries are lost to that caller).
