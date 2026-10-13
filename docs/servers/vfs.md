# The VFS server

[`servers/vfs`](../../servers/vfs) owns every process's file-descriptor table and decides, for each path, which backend answers it: a mounted filesystem server (f2fs), the in-VFS tmpfs, the `/dev` nodes, the synthesised `/proc` and `/sys/class/block` trees, or a static RamFS entry. It also implements the descriptor-only objects (pipes, PTY ends, eventfd, timerfd, signalfd, inotify) so that they behave like ordinary fds under `dup`, `fork`, `poll` and SCM_RIGHTS.

## Overview

| Area | Where | Summary |
|---|---|---|
| fd tables | `FD_TABLES` in [`lib.rs`](../../servers/vfs/src/lib.rs) | one `ProcFdTable` per process in a map keyed by TGID, created on first use and removed at exit; a sparse vector of 64-entry chunks plus an in-use bitmap (lowest free = a scan of bitmap words), bounded by the process's `RLIMIT_NOFILE` soft limit (a copy kept in the table, `set_fd_limit`) |
| open file descriptions | [`ofd.rs`](../../servers/vfs/src/ofd.rs) | the identity shared by `dup`/`fork`/SCM_RIGHTS copies; what epoll keys by |
| path resolution | [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) (`resolve_user_path`, `resolve_at_path`) plus each backend's walk | one kernel resolver for every path syscall; symlinks and permissions in the backend |
| mount routing | `MOUNTS`, `find_mount_port` | up to 8 mount servers (f2fs at `/` and `/data`) |
| tmpfs | `TMP_FILES`, `TMP_VMOS`, `TMP_OFDS` | `/tmp`, `/dev/shm`, `/run/user`; page-granular, budget = half of RAM |
| shared memory | memfd + tmpfs VMO promotion | `wl_shm` pools are shared memfd mappings |
| pipes | `PIPE_RINGS` | 128 rings of 16 KiB, endpoint-refcounted |
| `/proc` | `gen_proc_*` | synthesised into ephemeral snapshots at open time |
| `/sys/class/block`, `/dev/vd*`, loops | [`block.rs`](../../servers/vfs/src/block.rs) | see [filesystems.md](filesystems.md#block-devices) |

The fd table holds a 32-byte `VnodeKind` per descriptor (asserted at compile time: it is per-descriptor kernel memory). Its variants are the whole catalogue of things an fd can name: `RamFile`, `DevNull`, `DevZero`, `DevUrandom`, `DevFb`, `Pipe`, `Pty`, `TmpFile`, `EventFd`, `TimerFd`, `SignalFd`, `Inotify`, `DevStdio` (the console as reached through a descriptor), `DevVt` (`/dev/tty0..6`), `DynamicDevice` (DRM, evdev, PipeWire nodes, carrying a per-open cookie), `MountedFile` (a file on a mount server), `SockAlias` (a socket: names its net-server handle), `EpollFd` (an epoll fd: names its epoll handle), `BlockDev` and `SysBlock`. Sockets and epoll fds are entries of this table like everything else, so one table is the process's whole descriptor namespace; the kernel translates them to their handle before routing (see [README.md](README.md)). A new kernel object with its own table (a pidfd) joins the same way: a handle range, a `VnodeKind`, an arm in `handle_of` and in the kernel's routing. `/proc/self/fd` lists the caller's descriptors.

fd 0, 1 and 2 on the console are special: the kernel's read/write fast paths serve them directly and they never appear in any fd table. Their termios lives in the tty server ([tty-and-vt.md](tty-and-vt.md)).

## How it works

### Path resolution

**One resolver serves every path syscall.** `resolve_user_path` and `resolve_at_path` in [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) copy the user string, resolve it against the caller's cwd or its `dirfd`, normalise `.` and `..`, apply chroot confinement, and hand the VFS a pointer to a NUL-terminated absolute path in kernel memory (`KPath`, `KPATH_MAX` = 256).

- **Relative paths.** Before the resolver existed only `open`, `chdir`, `execve` and `pivot_root` resolved anything; every other path syscall forwarded the raw user pointer, so `cd /tmp; ls a.txt` failed while `ls /tmp/a.txt` worked.
- **`dirfd`.** `resolve_at_path` asks the VFS for the absolute path the dirfd was opened by (`VFS_FD_PATH`, the same query that answers `readlink("/proc/self/fd/N")`) and joins the relative path onto it. Ignoring `dirfd` broke every caller that opens a directory and works through it — GNU fts and `uucore::safe_traversal` (`rm`, `du`) — and `renameat` honouring its dirfds, not just its paths, was the difference between a clean COSMIC session log and a storm of `ENOENT`s from atomic state writes. When the descriptor has no nameable path the resolver falls back to the cwd rather than failing.
- **Empty path** is `ENOENT` (Linux), never "the cwd"; `AT_EMPTY_PATH` is handled by the callers that support it.
- **chroot.** An absolute path for a jailed task starts at the jail root, and `..` is clamped at a floor equal to the root's length. `fd_abs_path` strips the jail prefix from a dirfd's host-absolute path; the missing strip once made every `*at()` call inside a jail double-prefix and fail (`rm -r`, `ls -l`).
- **Symlink bodies are not resolved.** `symlink(2)`'s target is copied verbatim (`read_user_cstr_kpath`), so `ln -s ../x l` stays relative to the link's directory.

Symlink following and **permission checks happen in the backend's walk**, which is the only place that sees inodes: tmpfs's `tmp_resolve_links` and f2fs's `resolve_path_ex`. Both check search permission (`MAY_EXEC`) on every directory component *before* the lookup, so an unsearchable directory answers `EACCES` whether or not the leaf exists, return `ENOTDIR` for a non-directory mid-path, and `ELOOP` after the hop limit. See [security.md](security.md#filesystem-permission-enforcement).

### Routing a path

After resolution the VFS picks a backend in a fixed order (`should_lookup_ramfs`, then `find_mount_port`):

1. Intercepted namespaces: `/dev/…`, `/proc/…`, `/tmp/…`, `/etc/…`, `/run/user…` and `/sys/class/block/…`. These are answered by the VFS itself — tmpfs, device nodes, synthesised files, or static RamFS entries.
2. Everything else goes to the mount table. After `init` pivots the f2fs root onto `/`, `find_mount_port` matches every absolute path, which is exactly why the intercepts must run first.

Static RamFS entries **shadow** same-path files on the mounted root. The built-in `/etc/passwd` and `/etc/group` were removed for that reason once real ones were seeded on disk; `/etc/motd`, `/etc/hostname`, `/etc/hosts`, `/etc/resolv.conf` (rewritten by the DHCP client via `set_dhcp_dns`), `/etc/services`, `/etc/protocols`, `/etc/nsswitch.conf` and `/etc/os-release` still come from RamFS.

A **synthesised pseudo-filesystem must overlay only the subtree it owns.** The block layer's first version intercepted all of `/sys`, which hid the on-disk `/sys/dev/char/226:{0,128}` stubs that libdrm's `drmNodeIsDRM` and drm-rs stat to validate `/dev/dri/card0`. `cosmic-comp` logged "the provided file descriptor does not refer to a DRM node", exited, and the desktop went black on both architectures — while the disk tests that verified the block work all passed. The intercept now matches only `/sys/class/block`; `/sys` and `/sys/class` are staged on disk by [`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py). The rule: grep the image builder for a prefix before intercepting a path family — `/sys`, `/proc` and `/dev` all have on-disk stubs somewhere.

### Mounted files

A `MountedFile { port, file_id }` names an open file in a mount server's own open-file table. The VFS forwards reads, writes, `fstat`, `fsync`, xattr calls and `ioctl`s to `port`. Two rules keep this honest:

- **`fstat` is proxied** to the owning mount (`VFS_FSTAT`). It used to hardcode `S_IFREG`, so musl's `fdopendir` got `ENOTDIR` on directory fds.
- **Mount-side slots are refcounted** by `(port, file_id)` across every fd table. `close` used to free the mount's slot while a `dup` still referenced it; `read_dir`'s dup-then-close then killed the original directory fd.
- `O_APPEND` travels per write (a fourth message word), because the mount server owns the file position and `fcntl(F_SETFL)` can set the bit after the open was forwarded.

`mount(2)`, `umount2(2)`, `pivot_root(2)` and `/etc/fstab` (via `userland/fstab`, `userland/mount`, `userland/umount`) drive the mount table. `/proc/mounts`, `/etc/mtab` and `/proc/self/mountinfo` list the registered mounts and the in-VFS tmpfs roots (each with its own minor), so `df /tmp` reports `tmpfs`, not the root f2fs.

### Open file descriptions

[`ofd.rs`](../../servers/vfs/src/ofd.rs) gives every object Linux's *open file description* identity: an id allocated when an fd is created from nothing, copied with every fd copy, dropped with every retirement; ids carry a 16-bit generation so a reused slot never matches a stale id. It is shared by the VFS and the net server's socket tables. epoll keys its registrations by `(description, fd number)` and is told when the last reference of a watched description dies, so a registered fd that is closed while a forked child, a `dup` or an in-flight SCM_RIGHTS copy still holds the description keeps reporting — as on Linux. See [../kernel/event-loop.md](../kernel/event-loop.md).

tmpfs keeps its **file offset** per description too (`TMP_OFDS`). It used to live in the fd entry and was copied on `dup`/`fork`/SCM_RIGHTS, so every child of a redirected shell wrote from the parent's offset: `(seq 1 10000; seq 1 10) > /tmp/x` produced 48 894 bytes instead of 48 915, and a session log was only as long as its single largest writer — which looked like "stderr capture truncates silently". Writers now read and advance the shared offset under the tmpfs lock, so concurrent appenders do not overlap. Directory getdents cursors are still per descriptor.

### tmpfs

The tmpfs roots are `/tmp`, `/dev/shm` and `/run/user` (`TMPFS_ROOTS`). `/run/user` is the root rather than `/run/user/0`, so each `/run/user/<uid>` is an ordinary pool directory with its own owner and mode `0700`; the VFS seeds `/run/user/0` at boot and `init` seeds one per passwd account (see [security.md](security.md)). `/tmp` and `/dev/shm` are `1777`, `/run/user` is `0755 root`, and since creation is now permission-checked these modes are load-bearing.

- **Storage.** Regular-file bytes live in the inode's `TmpVmo`: a lazily-grown, page-granular frame list where a missing page is a hole. There is no per-file inline buffer any more; the old one capped every tmpfs file at 32 KiB and silently cut logs. Budget: `TMP_PAGES_USED` against half of physical memory (`ENOSPC`), 16 GiB per file (`EFBIG`), up to 512 entries (`MAX_TMP_FILES`).
- **Semantics.** Sparse files, `SEEK_DATA`/`SEEK_HOLE`, real `st_blocks`, `statfs` from the budget, `truncate`/`ftruncate`/`O_TRUNC` through one `tmp_set_size` (frees and zeroes the tail, honours `F_SEAL_SHRINK`), reads of up to 1 MiB per call, `fallocate` modes 0, `KEEP_SIZE` and `PUNCH_HOLE|KEEP_SIZE`, `atime` under Linux's `relatime` rule, full `atime`/`mtime`/`ctime`, xattrs in a 2 KiB per-inode arena.
- **AF_UNIX sockets bind only here.** A pathname `bind` creates an `S_IFSOCK` pool entry tagged with the net server's socket id (`unix_bind_node`), and `connect` resolves the path back through the same walk. f2fs has no `S_IFSOCK`, so a bind anywhere under `/` or `/data` returns `EOPNOTSUPP`. The socket-capable places are `/tmp`, `/dev/shm` and `/run/user` — worth knowing before pointing a daemon that hardcodes `/run` at this system; greetd's control socket and init's `/run/user/initctl` both live under `/run/user` for this reason.

### Shared memory, memfd and `wl_shm`

`memfd_create` backs each memfd with a tmpfs node named `/tmp/memfd:<name>` (created `O_EXCL`; a reused name gets a `:<seq>` suffix, because smithay names every `wl_shm` pool `smithay-client-toolkit` and a shared inode broke every winit/libcosmic client) and then **unlinks it**, so the memfd is anonymous and dies with its last fd, as on Linux. The node used to stay, burning one tmpfs slot per call and bricking `memfd_create` system-wide after about a hundred frames of a 1 Hz repainter.

`mmap(MAP_SHARED)` of a tmpfs or memfd fd maps the inode's VMO frames directly (`vmo_acquire_frames` → `map_shared_frames`): no copy-on-write, eager PTEs, and no `PROT_WRITE` requirement, so a compositor mapping a client's pool read-only aliases it. This is load-bearing for Wayland — `wl_shm` pools *are* shared memfd mappings passed over SCM_RIGHTS — and `memfd` seals (`F_ADD_SEALS`/`F_GET_SEALS`, `F_SEAL_SHRINK`) are honoured. `userland/scmtest`'s `test_shared_memfd_pixels` walks the same path and asserts that a write made after both mappings exist is visible on the other side. A long investigation of a frozen panel clock instrumented this path inside a live COSMIC session and proved it correct; the freeze was a kernel epoll bug (a nested epoll fd reporting `POLLNVAL` as readiness) and a panel redraw problem, not memory coherence.

PRIME-exported dmabufs use the same mechanism (`/tmp/dmabuf:<n>`, unlinked immediately after install).

### Pipes and descriptor-only objects

- **Pipes**: 128 rings (`MAX_PIPES`) of 16 KiB, `PIPE_BUF` = 4096 atomic writes. Each end is reference-counted, so EOF, `POLLHUP` and `EPIPE` fire only when the *last* fd on that end closes — which keeps shell pipelines correct across `fork`. `close` wakes the peer.
- **eventfd** (1024, global), **timerfd** (256, monotonic, `TFD_TIMER_ABSTIME`), **signalfd** and **inotify** (accepts watches, delivers nothing — enough to keep a config-watch source quiet). All honour `O_NONBLOCK` and `*_CLOEXEC` at creation and keep refcounted pool slots, so `dup`'d fds do not alias each other.
- **Readiness** is computed from real state (`VFS_POLL`): ring occupancy and endpoint counts, the eventfd counter, timer expiry. See [../kernel/event-loop.md](../kernel/event-loop.md).

### Devices and the console

- `/dev/null`, `/dev/zero` (whole-request reads, up to Linux's `MAX_RW_COUNT`), `/dev/urandom`, `/dev/fb0`; `lseek` on the null-like devices returns 0 so `dd of=/dev/null` works.
- `/dev/ptmx`, `/dev/pts/N`, `/dev/tty0..6`, `/dev/tty` → the tty server (see [tty-and-vt.md](tty-and-vt.md)).
- `/dev/dri/*`, `/dev/input/*`, PipeWire → `DynamicDevice` proxies to their servers with a per-open cookie (`open_id`), the VFS-side stand-in for Linux's `struct file`.
- **One identity for the console.** A dup of a raw console fd, `/dev/console`, `/dev/tty` without a controlling pty, and `/dev/std*` opened while that fd was the console all produce `DevStdio`: a *fixed* console object. It used to follow its source slot live, so `cmd 2>&1 >FILE` sent stderr into FILE and `/dev/console` wrote into whatever stdin had been redirected from. `/dev/stdin|stdout|stderr` now dup the object fd 0/1/2 holds at open time (BSD `/dev/fd` semantics). The console's single inode is what keeps `isatty` from lying.
- `stat` is arch-correct (the x86-64 and AArch64 `struct stat` layouts differ). `O_NOFOLLOW`, `O_DIRECTORY`, `O_PATH` and `AT_SYMLINK_NOFOLLOW` are honoured.

### /proc

`/proc` is **synthesised by the VFS** at open time into an ephemeral snapshot (the same mechanism as `/proc/meminfo`), not served by a separate process server.

- System files: `uptime`, `loadavg`, `meminfo`, `stat`, `cpuinfo`, `version`, `filesystems`, `mounts`, `kmemstat`, `vtstat`, `sys/kernel/*`, and `/proc/net/*` (generated by the net server, see [networking.md](networking.md#procnet)).
- Per process: `/proc` lists pids, `/proc/<pid>` opens as a directory, and `stat`, `status` (real `Uid`/`Gid` quadruples, `Vm*`/`Rss*`, `Threads`), `statm`, `cmdline`, `smaps` and `smaps_rollup` report real per-VMA accounting.
- `/proc/self/fd/N` resolves through `VFS_FD_PATH`; f2fs remembers the absolute path each open file was opened by.
- `/proc/<pid>/exe` resolves for any pid, not only `self` — but **only inside `readlink(2)`** (the kernel answers it from a TGID-keyed side table). Nothing `open`s or `execve`s that path, which is why greetd's session worker calls `std::env::current_exe()` before it forks rather than handing the magic path to `execv`.

### Synthetic /sys

Only `/sys/class/block` is synthesised (by [`block.rs`](../../servers/vfs/src/block.rs), from the live block registry — see [filesystems.md](filesystems.md#block-devices)). The rest of `/sys` — the DRM `/sys/dev/char` stubs, `/sys/bus/pci/devices`, and the skeleton the libudev shim reads — is staged on disk by the image builder.

### Open-file pools and exec images

- The f2fs server has 256 open files per mount (`MAX_OPEN_FILES`). **Directories take a slot too**: an icon-theme walk once starved a 32-slot pool and handed ten processes `EMFILE` within three seconds.
- `execve` keeps the executable's file open in a 64-entry `EXEC_FILES` table so pages can be faulted from it. When that fills (about 70 live processes with 20 terminals), new images are backed by inode through `MMAP_FILES` — keyed by `(mount, ino)`, pinned, with no f2fs open-file slot held — instead of reading the whole image into anonymous memory per process.

## Design decisions

- **Resolution in the kernel, symlinks and permissions in the backend.** Lexical work (cwd, dirfd, `.`/`..`, the chroot floor) needs only the caller's task state, so it is done once, in one place. Symlinks and modes need inodes, so the backend's walk owns them — and the jail root is read by the backend itself from `sched` (synchronous IPC, no protocol change), so a symlink inside a jail re-anchors at the jail root rather than climbing out.
- **A descriptor names an object, fixed when it is created.** Every variant that once followed a slot live (`DevStdio`) or copied a position that should have been shared (tmpfs offsets) was a POSIX violation with a visible symptom; both are now description-shaped.
- **Synthesise views of live state; never materialise them.** `/proc` and `/sys/class/block` are generated on open from the single source of truth, so a reader physically cannot observe a stale `size`.
- **Shape new fd kinds like existing ones.** `VnodeKind::Pty` is deliberately shaped like `VnodeKind::Pipe`, so a PTY end inherited every fd-table behaviour a pipe already had (dup2, fork, `O_NONBLOCK`, epoll, SCM_RIGHTS) instead of needing a second fd class; the dormant `TTY_FD_BASE` range was deleted rather than revived.

## Limitations and open work

- `/proc/self/fd/N` of an unlinked memfd reads its stale path without Linux's ` (deleted)` suffix; a console proxy reads `/dev/stdout` rather than the tty path.
- Directories keep a per-descriptor getdents cursor (no shared description).
- `CLOSE_RANGE_UNSHARE` in a multithreaded caller lets siblings see the closes.
- `KPATH_MAX` is 256; longer paths are not supported anywhere in the path plumbing.
- `MAX_MOUNTS` is 8; nothing mounts FAT or XFS partitions yet (there is no reader).
- `ls /dev/input` fails even though the event nodes open.
- Static RamFS entries under `/etc` still shadow any on-disk file of the same name.

## Code and tests

- [`servers/vfs/src/lib.rs`](../../servers/vfs/src/lib.rs) — fd tables, routing, tmpfs, pipes, `/proc`, devices.
- [`servers/vfs/src/ofd.rs`](../../servers/vfs/src/ofd.rs) — open file descriptions.
- [`servers/vfs/src/block.rs`](../../servers/vfs/src/block.rs) — block devices and `/sys/class/block`.
- [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) — `resolve_user_path`, `resolve_at_path`, `fd_abs_path`, `resolve_path`, `sys_memfd_create`, exec image tables.
- Tests: `userland/vfstest` (paths, dirfds, chroot, tmpfs sparse/large/append/shared-offset/ENOSPC, `fd_layout_512`, timestamps, `fallocate`, `/dev/zero`, mount table), `userland/exectest` (redirect shapes, `/dev/stdout` snapshot), `userland/scmtest` (shared memfd pixels, SCM_RIGHTS), `userland/permtest` (traversal and creation gates), `userland/memtest` (`/proc` accounting).
- `vfstest`'s `xattr_list_f2fs` and `chroot_confines_symlink_resolution` fail on a **re-run against the same image** (the test leaves residue on the persistent `/data` volume); always judge them on a fresh image.

## See also

- [README.md](README.md) — the server layer and resource lifecycle.
- [filesystems.md](filesystems.md) — f2fs, xattrs/ACLs, block devices.
- [security.md](security.md) — how permissions are enforced during resolution.
- [../kernel/syscalls.md](../kernel/syscalls.md) — the syscall personality and the path resolver's place in it.
- [../kernel/memory.md](../kernel/memory.md) — VMOs and shared mappings.
