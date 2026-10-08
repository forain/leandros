# Lane epollofd — 2026-10-01

**Outcome: FIXED.** Branch `lane/epollofd` @ `bfaed2e` (base origin/main `c28de13`).
Machines: Mac aarch64 HVF virgl; desktop x86_64 KVM virgl.

## What changed
- **`servers/vfs/src/ofd.rs` (new): open file descriptions.**
  - A global table of 16384 entries: refcount, generation, and a `watched` bit. An id is `gen<<16 | idx`; id 0 means none.
  - Leaf lock. The last `put` of a watched id calls the kernel's release hook outside the lock.
- **VFS `FdEntry.ofd` and net `SockEntry.ofd`.**
  - A new id is allocated at open, pipe, eventfd, timerfd, signalfd, inotify, pty peer, the stdio proxy, socket, socketpair and accept.
  - `get` on dup, dup2, F_DUPFD, fork (both tables) and SCM_RIGHTS export.
  - `put` on close, dup2 replace, exec cloexec sweep, close_all, steal_mounted_file and drop_transfer/xfer_drop.
  - This generalises tmpcap's TMP_OFDS (identity and lifetime), but does not replace it: TMP_OFDS still carries the tmpfs offset.
- **Epoll (`kernel/src/syscall.rs`).**
  - An interest stores `ofd`, and ADD/MOD/DEL match on (fd, ofd), like Linux's (file, fd).
  - The probe sends the expected ofd in VFS_POLL arg1 and NET_POLL arg2; the server answers ESTALE on a mismatch, so the fast path costs nothing extra.
  - If the owner's fd no longer names the description, `vfs::ofd_holder` / `net_server::ofd_holder` finds a dup, a fork child or an SCM receiver, and the result is cached in the interest (`ptgid`/`pfd`).
  - The release hook `epoll_ofd_released` removes the interests. An ADD that races the last close is caught by `mark_watched` under the EPOLL lock.
  - POLLNVAL is never reported. A reused number gets its own item.
  - termsegv's number-keyed release now applies only to interests with ofd 0 (untracked console fds, nested epoll fds, a full table).
- **close_range.**
  - Now covers VFS, socket and epoll fds, closing only the open ones.
  - CLOSE_RANGE_CLOEXEC is implemented. Before, it **closed** the fds instead of marking them.
  - CLOSE_RANGE_UNSHARE is accepted as a no-op. Tables are shared only between threads, so this is exact for a single-threaded caller.
  - EINVAL for a bad flag or first > last.
- **Epoll fds and close-on-exec.** Epoll fds now track FD_CLOEXEC (EPOLL_CLOEXEC, F_DUPFD_CLOEXEC, F_SETFD/F_GETFD), and execve closes the close-on-exec ones (`epoll_exec_cloexec`). Before this, every instance survived exec.
- **`MAX_SOCKS` 510 → 455.** SockEntry grew from 64 to 72 bytes, and the table must still fit one order-3 block (32 KiB assert).
- **Ctrl-T census.** `[EPOLL] ... ofds=live/16384`.

## Evidence
- **Linux reference.** The same C test (scratchpad `epollofd/epollofd.c`) on the desktop host, Linux 7.2.7: all expectations hold. One additional Linux-only behaviour: an fd that is in flight over SCM (queued, not yet received) also reports.
- **New polltest cases.** Both arches print exactly the Linux numbers:
  - fork `1 2 0 0 1 0`
  - dup `1 0`
  - scm `n1=1 n2=0`
  - close_range_epoll `0 1 0`
  - close_range `0 5 0 5 -22 -22`
- **The same polltest on the base kernel** (x86_64 KVM, c28de13): 4 FAIL.
  - fork `0 1 0 0 0 0`
  - scm `0 0`
  - close_range_epoll `0 0 0`
  - close_range `marked=0 closed=2 EINVAL=0`
  - dup passed on base too, via termsegv's alias move.
- **Suites (release, no GPU)** on both aarch64 HVF and x86_64 KVM: all RC=0.

  | suite | result |
  |---|---|
  | polltest | 13/0 |
  | epolltest | 11/0 |
  | wakepolltest | 17/0 |
  | vfstest | 59/0 |
  | scmtest | 36/0 |
  | exectest | 29/0 |
  | forktest | 8/0 |

- **COSMIC virgl, 8 cosmic-term open + 8 Super+Q close per session.**
  - aarch64 HVF: 3 sessions (24 terminals). x86_64 KVM: 2 sessions (16 terminals).
  - Each session also did 3 Settings open/close cycles.
  - **[PF]/SEGV = 0** in every session. termsegv's numbers were aarch64 0/24 and x86 2/48.
  - Screenshots show 8 windows, then an empty desktop.
- **Census** before → 8 open → after:

  | arch | ofds | epoll instances | fdtables | comp fds |
  |---|---|---|---|---|
  | aarch64 | 774 → 1137 → 771 | 118 → 206 → 118 | 30 → 46 → 30 | 104 → 172 → 102 |
  | x86_64 | 774 → 1135 → 779 | 118 → 206 → 118 | 30 → 46 → 30 | 104 → 170 → 110 |

  - fdtables returning to 30 means every terminal process exited: no hung exits.
  - The x86 comp fds residue (+6) is the known virgl retention noted by multiterm. It is not an ofd leak: ofds track fds.

## Deviations left (documented in code)
- A description held **only** by a queued SCM_RIGHTS message reports nothing until it is received. Linux reports it. The probe contributes the broadcast tag meanwhile, so no wake is lost.
- ADD on an existing (fd, ofd) still behaves as MOD (no EEXIST), and DEL of an unknown item returns 0 (no ENOENT). This lenience is pre-existing and was kept.
- Epoll fds are still not shared with a fork child (owner-tgid check). This is pre-existing.
- CLOSE_RANGE_UNSHARE in a multithreaded caller: siblings see the closes.

## Shared files touched
`kernel/src/syscall.rs` (epoll, close_range, execve sweep), `servers/vfs/src/lib.rs` + new `ofd.rs`, `servers/net/src/lib.rs` (SockEntry, MAX_SOCKS), `userland/polltest`.

Worktrees removed: Mac `~/code/leandros-epollofd`, and the desktop tmpfs `/tmp/epollofd` (including the base-check worktree).
