# Lane epollerr — 2026-10-02

**Outcome: FIXED.** Branch `lane/epollerr` @ `684e492` (base origin/main `d7d3297`; merges cleanly onto `a1f726e`).

Machines: Mac aarch64 HVF (virgl); Mac x86_64 **TCG**. The desktop dropped off the network mid-lane.

## Linux reference
Scratchpad `epollerr/epollerr.c` ran on the desktop host (Linux 7.2.7-arch1) **before** the outage, and all of its expectations held there:
- **ADD** on an existing (fd, file): `-17` (EEXIST).
- **MOD/DEL** of a missing item: `-2` (ENOENT).
- **ADD of the instance itself:** `-22` (EINVAL).
- **epfd not an epoll fd, or a bad op:** `-22` (EINVAL).
- **Closed target fd:** `-9` (EBADF).
- **Regular file, directory, /dev/null:** `-1` (EPERM).
- **Fork:** prints `child=15 p=1 q=0 cp=1 q-after-write=1 pdel=0 padd=0 child-after-parent-close=1`. The other fork checks print `lastref n=1 d=0x55 readd=-17` and `cloexec child=0 parent=1`.
- **In-flight SCM:** reporting was confirmed on Linux by lane epollofd (`inflight=1`).

Not confirmed on a real host: the polltest variant uses a `/tmp` (tmpfs) file as its regular file instead of `/etc/hostname`. Linux tmpfs has no poll, so EPERM is expected there, but the variant itself was never run on Linux.

## What changed
- **`sys_epoll_ctl`** now follows Linux `do_epoll_ctl`'s order:
  - EFAULT, then EBADF (either fd), then EPERM (not pollable), then EINVAL (not an epoll fd, or the instance itself), then EEXIST / ENOENT.
  - Interest-table full is now ENOSPC; it was ENOMEM.
  - MOD keeps the holder cache.
  - EPERM is checked up front only for ADD. MOD/DEL check it only when they find no item, so calloop's hot MOD re-arm path pays nothing extra.
  - The first 32 refusals are logged to serial as `[EPOLL] ctl err=N op= fd= tgid= <exe>`.
- **`vfs::fd_epoll_eperm`** answers EPERM for:
  - /dev/null, /dev/zero and /dev/fb0.
  - Block devices and `/sys/class/block` directories.
  - RamFS directories and named tmpfs files.
  - f2fs files outside `/sys`, `/proc` and `/dev`.
  - It deliberately allows procfs/sysfs snapshots, dmabufs (smithay polls them) and memfds.
- **Epoll fds are shared across fork.** `EpollFdTable` gives each number a primary holder inline plus extra holders (fork copies) in a Vec, each with its own FD_CLOEXEC.
  - All three fork paths call `epoll_fork_dup`.
  - close, exec cloexec, exit and close_range drop only the caller's hold.
  - An instance is freed when its last number goes. Its owner is re-homed if the creator leaves first.
  - Ctrl-T census shows `forkcopies=`.
- **In-flight SCM_RIGHTS.**
  - `net_server::inflight_vfs(ofd)` finds the queued `TransferFd`.
  - `vfs::poll_transfer` probes it through the new `poll_vnode`, which is `handle_poll`'s match factored out with no behaviour change.
  - The answer only counts if `ofd::live` still holds afterwards.
  - Queued *sockets* still report nothing; they use the broadcast tag.
- **polltest:** 4 new cases (errno, fork_shared, fork_lastref, fork_cloexec), and the scm case now checks the in-flight report.

## Evidence (both arches print the Linux numbers exactly)

| suite | aarch64 HVF | x86_64 TCG |
|---|---|---|
| polltest | 17/0 | 17/0 |
| epolltest | 11/0 | 11/0 |
| wakepolltest | 17/0 | 17/0 |
| scmtest | 43/0 | 43/0 |
| forktest | 8/0 | 8/0 |
| exectest | 29/0 | 29/0 |
| vfstest | 59/0 | 59/0 |

All suites returned RC=0.

**COSMIC sessions (virgl, 3× Settings, 8 cosmic-term open then Super+Q):** in every session, SEGV, [PF], panic and WDOG were all 0, and `[EPOLL] ctl err` was 0. **No COSMIC client hit EEXIST, ENOENT, EPERM or EBADF.**

| session | instances (before → 8 open → after) | fdtables (before → 8 open → after) |
|---|---|---|
| aarch64 #1 | 129 → 217 → 129 | 32 → 48 → 32 |
| aarch64 #3 | 118 → 206 → 118 | 30 → 46 → 30 |
| x86_64 TCG | 129 → 217 → 129 | 32 → 48 → 32 |

- The x86_64 TCG session used a 20 s delay per terminal.
- aarch64 #2 ran while an x86 build loaded the host. Only 3 of 8 windows had mapped when the screenshot was taken. Afterwards fdtables was +2 and instances +11. The QEMU had already stopped, so I could not tell whether these were late launchers or hung exits. Session #3, with the extra +40 s census, was exact.
- `forkcopies=0` throughout: no session client keeps an epoll fd across fork.

## Open
- **relibc `poll()`** (`../relibc/src/header/poll/mod.rs`) emulates poll with epoll and returns -1 on any ADD error other than EBADF.
  - poll() of a regular file, or a duplicate fd in the array, now fails there (Linux poll succeeds).
  - It only affects relibc-linked test binaries; musl userland uses the kernel's poll. I did not touch it (separate repo).
  - relibc `select()` already treats EPERM as "ready".
- **Not implemented:** ELOOP for epoll cycles, and the EPOLLEXCLUSIVE EINVAL rules.
- **Desktop cleanup:** only `/tmp/epollerr` and `/tmp/epollerr.c` on the host. No worktree or QEMU was started there.

Shared files touched: `kernel/src/syscall.rs` (epoll and fork paths), `servers/vfs/src/lib.rs` (`poll_vnode` refactor of `handle_poll`, `fd_epoll_eperm`, `TransferFd::ofd`), `servers/net/src/lib.rs` (`inflight_vfs`), `userland/polltest`.
