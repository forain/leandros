# The VFS server

[`servers/vfs`](../../servers/vfs) owns every process's file-descriptor table and decides, for each path, which backend answers it: a mounted filesystem server (f2fs), the in-VFS tmpfs, the `/dev` nodes, the synthesised `/proc` and `/sys/class/block` trees, or a static RamFS entry. It also implements the descriptor-only objects (pipes, PTY ends, eventfd, timerfd, signalfd, inotify) so that they behave like ordinary fds under `dup`, `fork`, `poll` and SCM_RIGHTS.

## Overview

| Area | Where | Summary |
|---|---|---|
| fd tables | `FD_TABLES` in [`lib.rs`](../../servers/vfs/src/lib.rs) | one `ProcFdTable` per process, each behind its own lock, in a map keyed by TGID (created on first use, removed at exit); a sparse vector of 64-entry chunks plus an in-use bitmap (lowest free = a scan of bitmap words), bounded by the process's `RLIMIT_NOFILE` soft limit (a copy kept in the table, `set_fd_limit`). See [Locking the fd tables](#locking-the-fd-tables) |
| open file descriptions | [`ofd.rs`](../../servers/vfs/src/ofd.rs) | the identity shared by `dup`/`fork`/SCM_RIGHTS copies; what epoll keys by |
| path resolution | [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) (`resolve_user_path`, `resolve_at_path`) plus each backend's walk | one kernel resolver for every path syscall; symlinks and permissions in the backend |
| mount routing | `MOUNTS`, `find_mount_port` | up to 8 mount servers (f2fs at `/` and `/data`) |
| tmpfs | `TMP_FILES`, `TMP_VMOS`, `TMP_OFDS` | `/tmp`, `/dev/shm`, `/run/user`, `/run`; page-granular, budget = half of RAM |
| shared memory | memfd + tmpfs VMO promotion | `wl_shm` pools are shared memfd mappings |
| pipes | `PIPE_RINGS` | 128 rings of 16 KiB, endpoint-refcounted |
| `/proc` | `gen_proc_*` | synthesised into ephemeral snapshots at open time |
| `/sys/class/block`, `/dev/vd*`, loops | [`block.rs`](../../servers/vfs/src/block.rs) | see [filesystems.md](filesystems.md#block-devices) |

The fd table holds a 32-byte `VnodeKind` per descriptor (asserted at compile time: it is per-descriptor kernel memory). Its variants are the whole catalogue of things an fd can name: `RamFile`, `DevNull`, `DevZero`, `DevUrandom`, `DevFb`, `Pipe`, `Pty`, `TmpFile`, `EventFd`, `TimerFd`, `SignalFd`, `Inotify`, `DevStdio` (the console as reached through a descriptor), `DevVt` (`/dev/tty0..6`), `DynamicDevice` (DRM, evdev, PipeWire nodes, carrying a per-open cookie), `MountedFile` (a file on a mount server), `SockAlias` (a socket: names its net-server handle), `EpollFd` (an epoll fd: names its epoll handle), `BlockDev` and `SysBlock`. Sockets and epoll fds are entries of this table like everything else, so one table is the process's whole descriptor namespace; the kernel translates them to their handle before routing (see [README.md](README.md)). A pidfd needs no handle: it is a plain `PidFd` entry of this table (it has no object table of its own), so it is numbered, dup'd, inherited, passed and closed like a pipe. A future kernel object that does keep its own table joins the way sockets and epoll did: a handle range, a `VnodeKind`, an arm in `handle_of` and in the kernel's routing. `/proc/self/fd` lists the caller's descriptors.

`fstat` on a socket or an epoll fd is answered by the kernel, which holds the handle: a socket is `S_IFSOCK | 0777` whose `st_ino` is its open file description id (shared by dups and fork copies, the number `/proc/net/*` lists, and what `readlink("/proc/self/fd/N")` shows as `socket:[ino]`); an epoll fd is an anonymous inode, mode `0600` with no type bits (`anon_inode:[eventpoll]`).

fd 0, 1 and 2 on the console are special: the kernel's read/write fast paths serve them directly and they never appear in any fd table. Their termios lives in the tty server ([tty-and-vt.md](tty-and-vt.md)).

## How it works

### Locking the fd tables

Each process's table has its own lock (lockwatch class `FD_TABLE`); `FD_TABLES` is only the TGID → table map, held to look up, insert or remove a table, and each CPU caches the table it looked up last, so a syscall's resolve-then-operate pair rarely touches the map. Until lane fdgaps (2026-10-13) one lock guarded every table, so every fd syscall of every process serialised on it: four processes running `fdmaxtest bench` side by side contended on 21 % of 3.9 M acquisitions and spun 292 ms; now the map sees 12.9 k acquisitions (7 contended) and the per-process locks 3.85 M (18 contended). `/proc/lockstat` prints these counters for every tracked lock.

- **Order**: `FD_COPY_GATE` → one table lock → the map → the leaf locks (`PIPE_RINGS`, `TMP_*`, `ofd`, …). No path holds two table locks: fork copies the parent's entries under the parent's lock and publishes the child's table whole.
- **Exit**: `handle_close_all` takes the table out of the map and marks it `dead` under its lock, so a caller that looked it up first and locked it after installs nothing (`TblSlot::holds`); a stale per-CPU cache entry is caught the same way.
- **Scans that decide "unreferenced"** (a mounted file's or a tmpfs inode's last close, unlink's open mask) must see every table at one instant, which the global lock used to give for free. A descriptor copied into another table — fork, `import_fd`, `export_fd`'s in-flight count — takes `FD_COPY_GATE` shared; such a scan takes it exclusive. A `dup` within one table needs no gate: the table's own lock orders it against the scan of that table.

### Path resolution

**One resolver serves every path syscall.** `resolve_user_path` and `resolve_at_path` in [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) copy the user string, resolve it against the caller's cwd or its `dirfd`, normalise `.` and `..`, apply chroot confinement, and hand the VFS a pointer to a NUL-terminated absolute path in kernel memory (`KPath`, heap-held).

- **Limits are Linux's.** A path string may be up to `PATH_MAX` = 4096 bytes including the NUL, a component up to `NAME_MAX` = 255; longer is `ENAMETOOLONG`. The host-absolute path the servers see can be longer than that (a jail root plus a `PATH_MAX` path, or a deep cwd), so every server-side path buffer is sized to `HOST_PATH_MAX` = 8192 (`vfs_server::HOST_PATH_MAX`) and heap-allocated. The task's cwd and root are `Arc<[u8]>` in `Task`. The remaining short limits are storage, not resolution: a tmpfs entry's path is at most 127 bytes and a tmpfs symlink body 256 (`ENAMETOOLONG` beyond), and a loop device's backing path 127.
- **Two forms of a path.** The resolver produces the *namespace* path (what the cwd, the chroot floor and `..` work on) and then the *backing* path the VFS serves, which differs only under an alias mount (below). `KPath::ns_bytes` is what `chdir`/`chroot` record; `KPath::bytes`/`ptr` is what the servers get.

- **Relative paths.** Before the resolver existed only `open`, `chdir`, `execve` and `pivot_root` resolved anything; every other path syscall forwarded the raw user pointer, so `cd /tmp; ls a.txt` failed while `ls /tmp/a.txt` worked.
- **`dirfd`.** `resolve_at_path` asks the VFS for the absolute path the dirfd was opened by (`VFS_FD_PATH`, the same query that answers `readlink("/proc/self/fd/N")`) and joins the relative path onto it. Ignoring `dirfd` broke every caller that opens a directory and works through it — GNU fts and `uucore::safe_traversal` (`rm`, `du`) — and `renameat` honouring its dirfds, not just its paths, was the difference between a clean COSMIC session log and a storm of `ENOENT`s from atomic state writes. When the descriptor has no nameable path the resolver falls back to the cwd rather than failing.
- **Empty path** is `ENOENT` (Linux), never "the cwd"; `AT_EMPTY_PATH` is handled by the callers that support it.
- **chroot.** An absolute path for a jailed task starts at the jail root, and `..` is clamped at a floor equal to the root's length. `fd_abs_path` strips the jail prefix from a dirfd's host-absolute path; the missing strip once made every `*at()` call inside a jail double-prefix and fail (`rm -r`, `ls -l`).
- **Alias mounts** ([`servers/vfs/src/alias.rs`](../../servers/vfs/src/alias.rs)). `/proc`, `/dev` and the synthesized `/sys` are path prefixes, not filesystems, so a chroot could not see them. `mount -t proc|devtmpfs|sysfs|devpts X DIR` and `mount --bind SRC DIR` (`MS_BIND`) add a prefix alias `DIR -> /proc` (or `/dev`, `/sys`, `/dev/pts`, `SRC`); the resolver applies it to the namespace path, and `umount` removes it. Paths reported back (`/proc/self/fd/N`, `/proc/self/exe`, `/proc/self/cwd`, `fchdir`, `getcwd`) are mapped back through the aliases and have the caller's jail stripped, so a jailed process reads them as it named them. `/proc/<pid>/root/...` and `/proc/<pid>/cwd/...` resolve through that process's root and cwd, and a final `/proc/<pid>/{exe,cwd,root,fd/N}` is followed by `open`/`stat`. The table is global (one mount namespace) and empty on a normal boot. `/proc/mounts` and `mountinfo` list aliases inside the reader's root.
- **`openat2`** reads `struct open_how` (size-versioned, `E2BIG` on unknown non-zero bytes). `RESOLVE_BENEATH`, `RESOLVE_IN_ROOT`, `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS` and `RESOLVE_NO_XDEV` are implemented by a kernel-side component walk (`resolve_scoped`: `lstat`/`readlink` per component), `RESOLVE_CACHED` resolves normally.
- **Symlink bodies are not resolved.** `symlink(2)`'s target is copied verbatim (`read_user_cstr_kpath`), so `ln -s ../x l` stays relative to the link's directory.

Symlink following and **permission checks happen in the backend's walk**, which is the only place that sees inodes: tmpfs's `tmp_resolve_links` and f2fs's `resolve_path_ex`. Both check search permission (`MAY_EXEC`) on every directory component *before* the lookup, so an unsearchable directory answers `EACCES` whether or not the leaf exists, return `ENOTDIR` for a non-directory mid-path, and `ELOOP` after the hop limit. See [security.md](security.md#filesystem-permission-enforcement).

### Routing a path

After resolution the VFS picks a backend in a fixed order (`should_lookup_ramfs`, then `find_mount_port`):

1. Intercepted namespaces: `/dev/…`, `/proc/…`, `/tmp/…`, `/etc/…`, `/run` and `/run/…` and `/sys/class/block/…`. These are answered by the VFS itself — tmpfs, device nodes, synthesised files, or static RamFS entries.
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

The tmpfs roots are `/tmp`, `/dev/shm`, `/run/user` and `/run` (`TMPFS_ROOTS`). `/run` is a tmpfs as on Linux: everything under it (pid and lock files, `/run/cosmic-greeter`, `/run/bluetooth`) is recreated at boot by the service that owns it, and the root volume's own `/run` directory is shadowed. `/run/user` stays a separate root nested in it (listed before `/run`, since the first matching root owns a path). `/run/user` is the root rather than `/run/user/0`, so each `/run/user/<uid>` is an ordinary pool directory with its own owner and mode `0700`; the VFS seeds `/run/user/0` at boot and `init` seeds one per passwd account (see [security.md](security.md)). `/tmp` and `/dev/shm` are `1777`, `/run` and `/run/user` are `0755 root`, and since creation is now permission-checked these modes are load-bearing.

- **Storage.** Regular-file bytes live in the inode's `TmpVmo`: a lazily-grown, page-granular frame list where a missing page is a hole. There is no per-file inline buffer any more; the old one capped every tmpfs file at 32 KiB and silently cut logs. Budget: `TMP_PAGES_USED` against half of physical memory (`ENOSPC`), 16 GiB per file (`EFBIG`), up to 512 entries (`MAX_TMP_FILES`).
- **Semantics.** Sparse files, `SEEK_DATA`/`SEEK_HOLE`, real `st_blocks`, `statfs` from the budget, `truncate`/`ftruncate`/`O_TRUNC` through one `tmp_set_size` (frees and zeroes the tail, honours `F_SEAL_SHRINK`), reads of up to 1 MiB per call, `fallocate` modes 0, `KEEP_SIZE` and `PUNCH_HOLE|KEEP_SIZE`, `atime` under Linux's `relatime` rule, full `atime`/`mtime`/`ctime`, xattrs in a 2 KiB per-inode arena.
- **Mapping is lazy and shared with the VMO.** `MAP_SHARED` of a tmpfs file or memfd creates only a VMA; pages come from the object on fault, 32 at a time ([memory.md](../kernel/memory.md#shared-tmpfs-and-memfd-mappings-vmos)). A mapping above 256 MiB is allowed (lane tmpfsbig): `MAP_PRIVATE` of such a file is copy-on-write (it once leaked writes into the file), truncate keeps CoW private pages, and fork keeps the CoW marks of aliased private maps. A read-only `MAP_PRIVATE` of a large file (a git packfile) no longer fails. `PROT_EXEC` `mmap`/`mprotect` on a `noexec` tmpfs is `EPERM`, and `statfs` reports the mount flags (lane noexec).
- **AF_UNIX sockets bind only here.** A pathname `bind` creates an `S_IFSOCK` pool entry tagged with the net server's socket id (`unix_bind_node`), and `connect` resolves the path back through the same walk. f2fs has no `S_IFSOCK`, so a bind anywhere under `/` or `/data` returns `EOPNOTSUPP`. The socket-capable places are `/tmp`, `/dev/shm`, `/run` and `/run/user` (btd listens on `/run/bluetooth/btd.sock`). Binding on f2fs would need an `S_IFSOCK` inode type in the f2fs server plus a VFS map from (dev, ino) to the live socket id; Linux allows it, nothing here needs it yet.

### Shared memory, memfd and `wl_shm`

`memfd_create` backs each memfd with a tmpfs node named `/tmp/memfd:<name>` (created `O_EXCL`; a reused name gets a `:<seq>` suffix, because smithay names every `wl_shm` pool `smithay-client-toolkit` and a shared inode broke every winit/libcosmic client) and then **unlinks it**, so the memfd is anonymous and dies with its last fd, as on Linux. The node used to stay, burning one tmpfs slot per call and bricking `memfd_create` system-wide after about a hundred frames of a 1 Hz repainter.

`mmap(MAP_SHARED)` of a tmpfs or memfd fd maps the inode's VMO frames directly (`vmo_acquire_frames` → `map_shared_frames`): no copy-on-write, eager PTEs, and no `PROT_WRITE` requirement, so a compositor mapping a client's pool read-only aliases it. This is load-bearing for Wayland — `wl_shm` pools *are* shared memfd mappings passed over SCM_RIGHTS — and `memfd` seals (`F_ADD_SEALS`/`F_GET_SEALS`, `F_SEAL_SHRINK`) are honoured. `userland/scmtest`'s `test_shared_memfd_pixels` walks the same path and asserts that a write made after both mappings exist is visible on the other side. A long investigation of a frozen panel clock instrumented this path inside a live COSMIC session and proved it correct; the freeze was a kernel epoll bug (a nested epoll fd reporting `POLLNVAL` as readiness) and a panel redraw problem, not memory coherence.

PRIME-exported dmabufs use the same mechanism (`/tmp/dmabuf:<n>`, unlinked immediately after install).

### Pipes and descriptor-only objects

- **Pipes**: 128 rings (`MAX_PIPES`) of 16 KiB, `PIPE_BUF` = 4096 atomic writes. Each end is reference-counted, so EOF, `POLLHUP` and `EPIPE` fire only when the *last* fd on that end closes — which keeps shell pipelines correct across `fork`. `close` wakes the peer.
- **eventfd** (1024, global), **timerfd** (256, monotonic, `TFD_TIMER_ABSTIME`), **signalfd** and **inotify** (see [inotify](#inotify)). All honour `O_NONBLOCK` and `*_CLOEXEC` at creation and keep refcounted pool slots, so `dup`'d fds do not alias each other.
- **pidfd** (`PidFd { pid }`): no pool, since pids are never reused; readable once the process has exited (see [../kernel/processes.md](../kernel/processes.md#pidfds)).
- **Readiness** is computed from real state (`VFS_POLL`): ring occupancy and endpoint counts, the eventfd counter, timer expiry, process exit. See [../kernel/event-loop.md](../kernel/event-loop.md).
- **splice / tee / vmsplice.** The kernel drives `splice` (`sys_splice`) through ring helpers that split the pipe read and write arms at the point where the other end's I/O happens: `pipe_peek` + `pipe_consume` on the read side (bytes a file or socket did not take stay queued), `pipe_reserve` + `pipe_commit` on the write side (ring space is reserved before the source is read and no other writer can take it meanwhile, so a socket or console byte read always has room — a writer racing into the same pipe used to make such a splice drop bytes; `syscalltest`'s `splice_socket_race`), `pipe_to_pipe` for two rings in one lock hold (also `tee`, which leaves the source intact). A splice returns as soon as it has moved anything — a full destination or an empty source pipe ends it short — and blocks only when it could move nothing (`EAGAIN` under `SPLICE_F_NONBLOCK` or an `O_NONBLOCK` pipe). The old `ENOSYS` stub existed because a move-everything loop deadlocked uutils `cat` on its own full broker pipe; the short count is what removes that. `off_in`/`off_out` give `pread`/`pwrite` semantics on the non-pipe end and `ESPIPE` on a pipe end. `vmsplice` is `readv`/`writev` on the pipe (no page gifting).

### inotify

[`inotify.rs`](../../servers/vfs/src/inotify.rs) is real, and on by default; the kernel command-line knob **`vfs.inotify=0`** is the kill switch (watches are then accepted but nothing fires, and the hooks cost one flag test). Watches live in the VFS and events are emitted from its own routing points (`inotify_hooks`) after an operation has succeeded, so no filesystem server needs to know about them. An instance is keyed by the id of its open file description (`VnodeKind::Inotify`), so `dup`, `fork` and `SCM_RIGHTS` share it and the last `ofd::put` frees it. `STATE` is a leaf lock and is never held across a wake; readiness is a keyed poll wake on the instance.

- **Coverage.** Directory-entry events on **tmpfs** (`IN_CREATE`, `IN_DELETE`, `IN_MOVED_FROM/TO`), `IN_DELETE_SELF` plus `IN_IGNORED` for a removed watched directory and `IN_IGNORED` on `inotify_rm_watch`. A watch on another filesystem (f2fs, `/proc`) is accepted and never fires. `IN_ONESHOT`, `IN_MASK_ADD`, `IN_MASK_CREATE`, `IN_ONLYDIR`, `IN_DONT_FOLLOW`, `IN_EXCL_UNLINK` and `IN_Q_OVERFLOW` are honoured; identical queued events are coalesced.
- **Limits.** Charged per (user namespace, kuid) up the namespace chain, like Linux `inc_ucount`: `max_user_instances` 128, `max_user_watches` 65536, `max_queued_events` 16384 (exceeding the queue delivers `IN_Q_OVERFLOW`), writable by root in `/proc/sys/fs/inotify/`, with read-only counters `events_queued`, `events_coalesced`, `events_overflow`, `instances` and `watches` beside them. A system-wide cap of 8192 instances backs them.
- **Why default-on.** The knob stayed off until the desktop was measured: with the full COSMIC session idle for two minutes, total CPU rose from 0.30 % to 0.46 % busy, with no events at idle and 35 instances / 54 watches; write and create/unlink microbenchmarks with 200 unrelated watches stayed within 3 %. Rootless `podman` and `pasta` need real notifications and `fanotify`-free config reload.

### Advisory locks

`fcntl` (POSIX and OFD) and `flock` records are keyed by **(mount server port, inode) with the owner being the thread group**, not by the mount server's open-file slot (which is one per `open(2)`). Keying by slot made two processes opening the same file each see an uncontended lock; SQLite then let two writers both hold EXCLUSIVE and each deleted the other's journal, which surfaced as `podman run --rm` exiting 127. Release happens by slot (closing a description drops the process's records on that inode, as POSIX specifies), waiters park on an exact 64-bit poll key (`lock_wake_key`) and releases wake only that key, so unrelated keyed wakes with a colliding bloom bit no longer wake every waiter (751 wakes per 750 round trips before).

### Other in-VFS filesystems

- **FUSE** ([`servers/fuse`](../../servers/fuse)): `/dev/fuse` (char 10:229), `mount -t fuse[.sub]` with `fd=N,rootmode=,user_id=,group_id=[,allow_other]`, protocol 7.31. The VFS is path based, so each operation walks with `LOOKUP` and sends `FORGET` afterwards; open files hold one lookup until `RELEASE`. Callers park until the daemon replies, the last close of the device fd aborts the connection (`ENOTCONN`), and without `allow_other` only the mounting uid may access it. A mount whose mount point lies under a tmpfs root (`/tmp/x`) is honoured by `open` and `getdents`.
- **POSIX message queues**: `mq_*` and `/dev/mqueue`, per IPC namespace ([`mqueue.rs`](../../servers/vfs/src/mqueue.rs)).
- **cgroup2**: [`cgroupfs.rs`](../../servers/vfs/src/cgroupfs.rs) serves `/sys/fs/cgroup` and any `mount -t cgroup2`; see [../kernel/namespaces.md](../kernel/namespaces.md#cgroup-v2).
- **overlayfs** is only partly built: the real-layer helper functions and a pseudo-pid credential override exist ([`ovl/`](../../servers/vfs/src/ovl)) but `mount -t overlay` is not wired.

### Mount namespaces

Processes in a non-initial mount namespace are resolved against a per-namespace mount tree (`mnt.rs`) instead of the alias table; processes in namespace 0 never touch it. The model (tree, propagation, `pivot_root`, `open_tree`/`move_mount`/`mount_setattr`, per-mount `ro`/`nodev`/`noexec`) is described in [../kernel/namespaces.md](../kernel/namespaces.md#mount-namespaces).

### Devices and the console

- `/dev/null`, `/dev/zero` (whole-request reads, up to Linux's `MAX_RW_COUNT`), `/dev/urandom`, `/dev/fb0`; `lseek` on the null-like devices returns 0 so `dd of=/dev/null` works.
- `/dev/ptmx`, `/dev/pts/N`, `/dev/tty0..6`, `/dev/tty` → the tty server (see [tty-and-vt.md](tty-and-vt.md)).
- `/dev/dri/*`, `/dev/input/*`, PipeWire → `DynamicDevice` proxies to their servers with a per-open cookie (`open_id`), the VFS-side stand-in for Linux's `struct file`.
- **`/dev/tty`** is the caller's controlling terminal: its pty (a VT session's included), the console for the serial login's session (the one that took the console with `TIOCSCTTY`), and `ENXIO` for a session with none, as on Linux. Until 2026-10-18 a session without one got the console proxy, whose termios answered for whatever fd 0 was; sudo-rs needed a patch for that.
- **One identity for the console.** A dup of a raw console fd, `/dev/console`, `/dev/tty` in the console's session, and `/dev/std*` opened while that fd was the console all produce `DevStdio`: a *fixed* console object. It used to follow its source slot live, so `cmd 2>&1 >FILE` sent stderr into FILE and `/dev/console` wrote into whatever stdin had been redirected from. `/dev/stdin|stdout|stderr` now dup the object fd 0/1/2 holds at open time (BSD `/dev/fd` semantics). The console's single inode is what keeps `isatty` from lying.
- `stat` is arch-correct (the x86-64 and AArch64 `struct stat` layouts differ). `O_NOFOLLOW`, `O_DIRECTORY`, `O_PATH` and `AT_SYMLINK_NOFOLLOW` are honoured.

### /proc

`/proc` is **synthesised by the VFS** at open time into an ephemeral snapshot (the same mechanism as `/proc/meminfo`), not served by a separate process server.

- System files: `uptime`, `loadavg`, `meminfo`, `stat`, `cpuinfo`, `version`, `filesystems`, `mounts`, `kmemstat`, `vtstat`, `sys/kernel/*`, and `/proc/net/*` (generated by the net server, see [networking.md](networking.md#procnet)). Kernel counters — `lockstat`, `rqhold`, `sysstat`, `wakestat`, `pcsample` — are described in [../development/debugging.md](../development/debugging.md#procfs-counters).
- Per process: `/proc` lists thread-group ids only (as Linux), `/proc/<pid>` opens as a directory, and `stat`, `status` (real `Uid`/`Gid` quadruples, `Vm*`/`Rss*`, `Threads`), `statm`, `cmdline`, `environ`, `comm`, `maps`, `smaps` and `smaps_rollup` report real per-VMA accounting; `maps`/`smaps` name each mapping as Linux does (the path a file was first mapped or executed through, `/memfd:<name> (deleted)`, `[heap]`, `[stack]`; no vDSO exists) and are never cut short (until 2026-10-19 `maps` read as 0 bytes and smaps stopped at 256 KiB). `/proc/<tid>` of a non-leader thread also resolves. `stat(2)` answers for every procfs path (directories 0555, files 0444, `comm` 0644, owned by the process's euid/egid).
- **Threads**: `/proc/<pid>/task/` lists every tid of the group, and each `task/<tid>/` holds the same files for that thread. Per-thread `stat` carries the tid, the thread's name and state and its own `utime`/`stime`; the process-level files keep the group sums. `/proc/self/task` and `/proc/thread-self[/<file>]` work. A tid of another group is `ENOENT` under `task/`.
- **Names** (`comm`): `/proc/<pid>/comm` and `task/<tid>/comm` read the thread's name (at most 15 bytes plus a newline) from the same tid-keyed table as `prctl(PR_SET_NAME/PR_GET_NAME)`, `stat`'s `(comm)` and `status`'s `Name:`. Writing it renames the thread (root or the same euid; checked at open and again at write; one trailing newline is stripped). An unnamed thread is its executable's basename; kernel threads carry their own names (`khungtaskd`, `netd`, `kiostat`, `klogd`, `netwatch`, `f2fs-pcache`, else `kthread`).
- **`cmdline` and `environ`** are read from the target's own stack through its address space: `execve` records where it laid out the argument and environment strings, so a program that retitles itself in place (setproctitle) shows the new title, including Linux's rule for a title that runs past the argument area into the environment. Capped at 64 KiB. Kernel threads have an empty `cmdline`; a user process with no recorded area (the boot-loaded PID 1) shows its executable path. `environ` is readable by root and the owner only.
- **`stat`** fields follow Linux: field 7 `tty_nr` is the controlling terminal as `makedev(major, minor)` — `pts/N` = (136 + N/256, N%256), VT N = (4, N), the serial console = (5, 1), from `tty_server::ctty_dev_of_sid` — and field 8 `tpgid` is that terminal's foreground process group; with no controlling terminal they are 0 and -1. Field 4 `ppid` is the parent *process* for every thread (the leader's parent, named by its tgid, as `getppid` reports; a thread's own record names the thread that created it). Field 22 `starttime` is the named task's creation time in clock ticks: the leader's for `/proc/<pid>/stat`, the thread's own for `task/<tid>/stat` and `/proc/<tid>/stat` (it was the current uptime until 2026-10-17, which broke sudo's credential cache, keyed on the parent's start time). `utime`/`stime` split user and in-syscall time (see [../kernel/scheduler.md](../kernel/scheduler.md#dispatch)) and keep the time of exited threads. Fields 10–13 are the address space's page faults (`minflt`: no device I/O, page-cache hits included; `majflt`: the fault's read reached a block device; `task/<tid>/stat` shows the thread's own) and those of waited-for children; 16–17 `cutime`/`cstime` are the waited-for children's CPU time, their own children's included (`sched::ChildAcct`, also `times(2)` and `getrusage(RUSAGE_CHILDREN)`).
- **Device numbers.** `st_rdev` of the terminal and memory nodes is Linux's, by path and by fd, and matches `tty_nr`: `/dev/pts/N` 136+N/256:N%256, `/dev/ptmx` and a pty master 5:2, `/dev/ttyN` 4:N, `/dev/console` 5:1, `/dev/tty` 5:0 (with a pty controlling terminal, the pty's), `/dev/null` 1:3, `/dev/zero` 1:5, `/dev/random` 1:8, `/dev/urandom` 1:9, `/dev/fb0` 29:0.
- **`prctl(PR_SET_MM, PR_SET_MM_{ARG,ENV}_{START,END})`** moves what `cmdline`/`environ` read, with Linux's rules: root only (CAP_SYS_RESOURCE, else `EPERM`), `arg4`/`arg5` zero and the address at least 4096 and in user space (`EINVAL`), the area kept ordered (`EINVAL`), some mapping at or above the address (`EFAULT`). As on Linux, a title only runs past `arg_end` when the environment starts right there. The other `PR_SET_MM` fields are still accepted and ignored.
- `/proc/self/fd/N` resolves through `VFS_FD_PATH`; f2fs remembers the absolute path each open file was opened by. **`open("/proc/<pid>/fd/N")` reopens the target's description** (lanes procfdx and procfdkind, 2026-10-23), including pipes (the kind of fd is carried so that the reopen picks the right object).
- **`/proc/cmdline` is redacted for every reader** (lane cmdlinepsk, `servers/vfs/src/cmdline_redact.rs`). A knob is secret when the last dot-separated component of its key is one of a fixed list or ends with a suffix such as `psk`, `password`, `token`, `key`; its value reads back as `<redacted len=N>` (key and length stay, no value bytes). The kernel's own console echo of the command line is redacted by the same rule, because the console is copied to the log partition. Wi-Fi secrets therefore never reach `/proc/cmdline`, the klog or `remote.py logs`.
- `/proc/<pid>/exe` resolves for any pid, not only `self` — but **only inside `readlink(2)`** (the kernel answers it from a TGID-keyed side table). Nothing `open`s or `execve`s that path, which is why greetd's session worker calls `std::env::current_exe()` before it forks rather than handing the magic path to `execv`.

### Synthetic /sys

Only `/sys/class/block` is synthesised (by [`block.rs`](../../servers/vfs/src/block.rs), from the live block registry — see [filesystems.md](filesystems.md#block-devices)). Since the laptop power work, [`sysclass.rs`](../../servers/vfs/src/sysclass.rs) also serves **`/sys/class/power_supply`, `/sys/class/backlight`, `/sys/class/rtc` and `/sys/power`** from an `Ops` table the kernel registers (the VFS cannot depend on the drivers). It is narrow on purpose, because the 2026-09-15 `/sys` shadowing regression blacked the desktop out: a class directory is overlaid only while a table is registered **and** reports at least one device in that class; otherwise every path resolves exactly as the on-disk `/sys`. Writing `backlight/*/brightness` is allowed for root and for members of group `video` (gid 44, effective or supplementary): the file is stat'ed `0664 root:video`, `max_brightness` and every other attribute stay root-only. This is what lets `login1` (running as the session user) set the brightness from the keyboard keys (lane bkperm, 2026-10-30; `permtest` cases `backlight_nogroup`/`backlight_group`, skipped where no `intel_backlight` exists). The rest of `/sys` — the DRM `/sys/dev/char` stubs, `/sys/bus/pci/devices`, and the skeleton the libudev shim reads — is staged on disk by the image builder.

### Open-file pools and exec images

- The f2fs server has 256 open files per mount (`MAX_OPEN_FILES`). **Directories take a slot too**: an icon-theme walk once starved a 32-slot pool and handed ten processes `EMFILE` within three seconds.
- `execve` keeps the executable's file open in a 64-entry `EXEC_FILES` table so pages can be faulted from it. When that fills (about 70 live processes with 20 terminals), new images are backed by inode through `MMAP_FILES` — keyed by `(mount, ino)`, pinned, with no f2fs open-file slot held — instead of reading the whole image into anonymous memory per process.

## Design decisions

- **Resolution in the kernel, symlinks and permissions in the backend.** Lexical work (cwd, dirfd, `.`/`..`, the chroot floor) needs only the caller's task state, so it is done once, in one place. Symlinks and modes need inodes, so the backend's walk owns them — and the jail root is read by the backend itself from `sched` (synchronous IPC, no protocol change), so a symlink inside a jail re-anchors at the jail root rather than climbing out.
- **A descriptor names an object, fixed when it is created.** Every variant that once followed a slot live (`DevStdio`) or copied a position that should have been shared (tmpfs offsets) was a POSIX violation with a visible symptom; both are now description-shaped.
- **Synthesise views of live state; never materialise them.** `/proc` and `/sys/class/block` are generated on open from the single source of truth, so a reader physically cannot observe a stale `size`.
- **Shape new fd kinds like existing ones.** `VnodeKind::Pty` is deliberately shaped like `VnodeKind::Pipe`, so a PTY end inherited every fd-table behaviour a pipe already had (dup2, fork, `O_NONBLOCK`, epoll, SCM_RIGHTS) instead of needing a second fd class; the dormant `TTY_FD_BASE` range was deleted rather than revived.

## Limitations and open work

- procfs gaps: children reaped by an auto-reaping parent (SIGCHLD ignored) are not added, as on Linux, and neither is a thread that exits after its leader was reaped.
- `/proc/self/fd/N` of an unlinked memfd reads its stale path without Linux's ` (deleted)` suffix; a console proxy reads `/dev/stdout` rather than the tty path.
- Directories keep a per-descriptor getdents cursor (no shared description).
- `CLOSE_RANGE_UNSHARE` in a multithreaded caller lets siblings see the closes.
- `KPATH_MAX` is 256; longer paths are not supported anywhere in the path plumbing.
- `MAX_MOUNTS` is 8; nothing mounts FAT or XFS partitions yet (there is no reader).
- `ls /dev/input` fails even though the event nodes open.
- Static RamFS entries under `/etc` still shadow any on-disk file of the same name.
- **Every f2fs mount reports `st_dev` 1**, so `cp -a` from one f2fs volume to another fails with "are the same file" when inode numbers coincide (volumes cloned from one image). Copy by content until devices get distinct numbers (found by lane lxmigrate).

## Code and tests

- [`servers/vfs/src/lib.rs`](../../servers/vfs/src/lib.rs) — fd tables, routing, tmpfs, pipes, `/proc`, devices.
- [`servers/vfs/src/ofd.rs`](../../servers/vfs/src/ofd.rs) — open file descriptions.
- [`servers/vfs/src/block.rs`](../../servers/vfs/src/block.rs) — block devices and `/sys/class/block`.
- [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) — `resolve_user_path`, `resolve_at_path`, `fd_abs_path`, `resolve_path`, `sys_memfd_create`, exec image tables.
- Tests: `userland/vfstest` (paths, dirfds, chroot, tmpfs sparse/large/append/shared-offset/ENOSPC, `fd_layout_512`, timestamps, `fallocate`, `/dev/zero`, mount table), `userland/exectest` (redirect shapes, `/dev/stdout` snapshot), `userland/scmtest` (shared memfd pixels, SCM_RIGHTS), `userland/permtest` (traversal and creation gates), `userland/memtest` (`/proc` accounting), `userland/ps` (`pstest`: `comm`, `task/`, `cmdline`/`environ`, `tty_nr`/`tpgid`, `ps` options; `--suite procps`).
- `vfstest`'s `xattr_list_f2fs` and `chroot_confines_symlink_resolution` fail on a **re-run against the same image** (the test leaves residue on the persistent `/data` volume); always judge them on a fresh image.

## See also

- [README.md](README.md) — the server layer and resource lifecycle.
- [filesystems.md](filesystems.md) — f2fs, xattrs/ACLs, block devices.
- [security.md](security.md) — how permissions are enforced during resolution.
- [../kernel/syscalls.md](../kernel/syscalls.md) — the syscall personality and the path resolver's place in it.
- [../kernel/memory.md](../kernel/memory.md) — VMOs and shared mappings.
