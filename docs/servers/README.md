# The server layer

Everything above the microkernel core that a Linux program would call "the kernel's subsystems" — files, sockets, terminals, `/proc`, the display and input devices — is implemented as a set of **servers**: crates under [`servers/`](../../servers) that answer IPC messages. This page explains how a server is wired in, which servers exist, the rules they share, and how their resources are torn down when a process dies.

## Overview

| Server | Crate | Serves | Protocol | Page |
|---|---|---|---|---|
| `vfs` | [`servers/vfs`](../../servers/vfs) | the fd tables, mount routing, tmpfs (`/tmp`, `/dev/shm`, `/run/user`), `/dev`, `/proc`, `/sys/class/block`, pipes, eventfd/timerfd/signalfd/inotify, block devices | `VFS_*` tags (`open`/`read`/`write`/`close`/`ioctl`/…) | [vfs.md](vfs.md) |
| `f2fs` | [`servers/f2fs`](../../servers/f2fs) | `/` and `/data` | the same `VFS_*` tags, forwarded by the VFS | [filesystems.md](filesystems.md) |
| `xattr` | [`servers/xattr`](../../servers/xattr) | not a server: the shared xattr/ACL/permission contract crate | — | [filesystems.md](filesystems.md#extended-attributes-and-posix-acls), [security.md](security.md) |
| `net` | [`servers/net`](../../servers/net) | AF_INET (smoltcp, DHCPv4, ICMP), AF_UNIX, `/proc/net/*` | `NET_*` tags | [networking.md](networking.md) |
| `tty` | [`servers/tty`](../../servers/tty) | console termios, job control, PTYs, the six VTs and their keyboard, POSIX timers | `TTY_IOCTL`, `TIMER_*`, plus direct calls | [tty-and-vt.md](tty-and-vt.md) |
| `drm` | [`servers/drm`](../../servers/drm) | `/dev/dri/card0`, `/dev/dri/renderD128` | Linux DRM ioctls carried as VFS ioctl messages | [../graphics/drm-kms.md](../graphics/drm-kms.md) |
| `evdev` | [`servers/evdev`](../../servers/evdev) | `/dev/input/eventN` | Linux `input_event` reads | [../input.md](../input.md) |
| `pipewire` | [`servers/pipewire`](../../servers/pipewire) | `/run/pipewire/pipewire-0` | PCM write, ioctl control | [../audio.md](../audio.md) |

Two more crates live under `servers/` but are not servers in the running system:

- **`servers/proc`** is a dormant early design (a process server owning PID→port mapping, wait4 zombies, process groups and sessions). Nothing links it into the kernel. Process groups, sessions and credentials live in the scheduler (`sched`), and **`/proc` is synthesised by the VFS** (see [vfs.md](vfs.md#proc)). Older documentation that lists a `proc` server serving `/proc` describes this unused crate.
- **`servers/libc-shim`** is a user-space runtime helper (auxv parsing, `getauxval`, raw IPC helpers for bare binaries), not a server.

## How it works

### A server is a port with a direct handler

Each server registers an IPC port and attaches a handler function to it with `ipc::port::register_handler` ([`ipc/src/port.rs`](../../ipc/src/port.rs)). When anything sends to a port that has a handler, `port::send` **calls the handler synchronously, on the sending task's own thread**, passing the caller's pid, and then posts the reply to the caller's reply port. There is no server thread, no queue to drain and no context switch on the common path.

This is the single most consequential property of the server layer:

- **The caller's identity is free.** A handler receives `caller_pid` and can read the caller's credentials, cwd, chroot root and umask straight from `sched` (`sched::euid_of`, `sched::current_root`, …). Credential enforcement, chroot confinement and the umask never needed a protocol change — they were added by reading state the handler already had access to.
- **Kernel pointers work across servers.** The kernel resolves every path into a kernel-resident buffer (`KPath`, see [vfs.md](vfs.md#path-resolution)) and hands the VFS a pointer to it. Kernel memory is mapped in every address space and the caller stays blocked for the whole round trip, so the buffer outlives every server that reads it.
- **Re-entrancy is real.** A loop device's I/O re-enters the VFS through `call_port` to the mount that owns the backing file, on the same thread. Every server therefore follows a strict lock discipline (below).
- **A server is a library.** The servers are ordinary `no_std` crates linked into the kernel ([`kernel/Cargo.toml`](../../kernel/Cargo.toml)); several are also called directly as functions where a message would add nothing (the tty server's console line discipline, the VFS poll queries).

Ports are discovered without a name service: the kernel stamps server port ids into every task's ELF auxiliary vector, so `get_vfs_port()`, `get_audio_port()` and friends are O(1) reads (DRM is reached through `/dev/dri`, not a port) at start-up. See [../kernel/ipc.md](../kernel/ipc.md).

### Linux personality on top

User programs do not speak `VFS_*` or `NET_*`. They make real Linux syscalls with the real per-arch numbers, and the dispatcher in [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) translates each into the message for the owning server.

Every descriptor of a process lives in **one** table, the VFS's per-process fd table, whatever its kind: a socket is a `VnodeKind::SockAlias` entry naming a net-server *handle*, an epoll fd an `EpollFd` entry naming an epoll handle. So every kind takes the lowest free number and counts against `RLIMIT_NOFILE` (soft 1024, hard 524288, `/proc/sys/fs/nr_open` 1048576). Handles live above every descriptor number and are never descriptors themselves; the dispatcher translates a descriptor to what it names once per syscall (`fd_handle_args`) and routes by the handle:

| number | Owner |
|---|---|
| `0 .. nr_open` | descriptors: the per-process VFS fd table |
| `0x0100_0000 ..` | socket handles (`vfs::SOCK_HANDLE_BASE` + slot of the process's net-server socket table) |
| `0x0200_0000 ..` | epoll handles (`vfs::EPOLL_HANDLE_BASE` + index into the kernel's epoll fd table) |

(Until lane fdmax, 2026-10-12, the descriptor number itself was the route: VFS fds below 0x200, sockets from 0x200, epoll fds from 0x400, each range with its own cap.) See [../kernel/syscalls.md](../kernel/syscalls.md).

### Layering seams

Servers sit *below* the device layer. The VFS must not depend on the `drivers` crate (that edge would be a cycle in spirit), and the tty server must not depend on `drivers` either (the framebuffer console is a driver; the VT layer is policy above it). Where they need a device, the device is handed to them:

- **vtables registered at boot** — the block backend reaches the VFS as a `&'static DiskOps` registered from [`kernel/src/init.rs`](../../kernel/src/init.rs); a null registration means "no block backend on this build" and every block entry point answers `ENXIO` instead of inventing a device. The DRM dmabuf release hook uses the same shape.
- **`extern "C"` symbols** — the VT layer calls `fb_vt_repaint_begin`, `kernel_console_winsize`, `evdev_vt_activated` and friends through `#[no_mangle]` seams.
- **function-pointer hooks** — `sched` cannot call the VFS, so the kernel registers the exit-teardown hook with `sched::register_exit_teardown_hook` (below); the VFS registers the epoll release hook on `vfs::ofd` the same way.

## Design decisions

### Synchronous in-caller dispatch rather than server threads

- **Choice.** Handlers run in the caller's context under the caller's pid.
- **Why.** It removes a context switch and a scheduler wake from every file and socket operation, and it gives every handler the caller's credentials without a credential-passing protocol. The 2026-07 credentials and chroot work confirmed that no VFS or f2fs message change was needed for either: the f2fs dispatcher had simply been discarding the `caller_pid` it was already given.
- **Cost.** A handler may not block on anything that the caller's own progress is needed to release, and every lock is held across a call that may re-enter. The lock discipline below is the price.

### Leaf locks, and nothing held across re-entry or user memory

- Every server lock is either a **leaf** (nothing else is taken while it is held) or has a documented order (`SOCK_TABLES` → `UNIX_CONNS` → `BOUND_PATHS` in the net server; `FD_TABLES`/`SOCK_TABLES` → `vfs::ofd` leaf).
- Nothing is held across a VFS re-entry (`call_port`) or a touch of user memory. User memory is copied through `AddressSpace::read_user_buf`/`write_user_buf`, never dereferenced, because a supervisor-mode fault on a not-yet-faulted copy-on-write page has no recovery path and a fault under the scheduler lock freezes every vCPU. See [../kernel/invariants.md](../kernel/invariants.md).
- Hot paths that can run in IRQ context (`vt::console_out`, the UART drain) only `try_lock` and count a drop when they lose.

### Fixed tables, sized from measurement, that say when they are full

Servers used fixed-size static pools, kept in `.bss` (an all-zero image) wherever possible. A COSMIC desktop with many terminals turned out to be the probe that found every ceiling, one after another — each fix exposed the next (eventfds → tasks → the compositor's per-process fd table → tmpfs → epoll → ptys). Since lane fdmax the descriptor-related ones grow on the heap as they are used, up to a ceiling that only bounds a runaway. The current sizes and the evidence behind them:

| Table | Size | Notes |
|---|---|---|
| processes / tasks | 512 / 1024 | `sched::runqueue::MAX_PROCESSES` sizes the console termios and timer tables; the fd and socket tables are allocated per process on demand |
| descriptors per process | `RLIMIT_NOFILE` (soft 1024, hard 524288, up to nr_open 1 Mi) | one table for every kind; 64-entry chunks + in-use bitmap, so memory follows use (was 512 VFS fds + 455 sockets, fixed) |
| socket slots per process | grows with use | heap vector; each socket is also one descriptor |
| AF_UNIX connections (global) | 512 | a session holds ~110; each terminal adds ~5 |
| eventfds / timerfds (global) | grow to 256 Ki / 64 Ki | an empty desktop used 235 eventfds (the fixed 1024 / 256 they replace) |
| epoll instances / interests / epoll handles | grow to 1 Mi / 1 Mi per instance / 16 Mi | were 1024 / 512 / 2048 fixed (~20 MiB of `.bss`) |
| pipes | grow to 16384; a 16 KiB ring per pipe with an end open | 128 fixed rings before; 16 once, which `ENFILE`d ~14 piped session components |
| tmpfs entries | 512 | file bytes live in page-granular VMOs, budget = half of RAM |
| open file descriptions | grow to 1 Mi | `vfs::ofd`; ids are 20 bits of slot + 12 of generation |
| PTY pairs | 64 | each window or tab holds one |
| f2fs open files per mount | grow to 16384 (256 at a time) | directories take a slot; 32 starved an icon-theme walk, 256 a build |
| advisory locks / device opens | grow to 64 Ki each | were 64 / 1024 |
| private-mmap inodes | grow to 1 Mi | was 2048; rustc maps every rlib it reads |

A full pool returns the errno Linux would (`ENFILE` for global pools, `EMFILE` per process, `ENOSPC` for tmpfs and `ptmx`) and **prints one serial line the first time** (`[VFS] pid N: fd table FULL at 1024 (EMFILE) -- this process's RLIMIT_NOFILE`, `[NET] unix connection pool FULL`, …). Silent exhaustion cost real time: a full eventfd pool surfaced as a second `cosmic-term` that forked and never drew, and a full compositor fd table as a blank 20th window that ignored `Super+Q`. The Ctrl-T task dump prints the occupancy of every pool (see [../development/debugging.md](../development/debugging.md)).

## Resource lifecycle

Server-side handles are released on **every** path out of a task, not only a clean `exit`.

- `sched::exit` is the single funnel every death path goes through — the `exit`/`exit_group` syscalls, a default-action fatal signal, a fatal user fault, and the signal-frame failure paths. The kernel registers `vfs_close_all_for` ([`kernel/src/syscall.rs`](../../kernel/src/syscall.rs)) as the scheduler's exit-teardown hook, so the VFS fd table, the socket table, tty handles and epoll instances are all released whichever path killed the task. Before the hook existed, a process killed by a signal while holding a pipe write end left the ring's writer count non-zero forever, wedging the reader and leaking one of (then) 16 pipe slots.
- `exit_group` releases a *sibling* thread's resources too, and per-process tables are keyed by **TGID**, never by the raw pid of whichever thread called — a class of bug that was fixed at least six times before it stopped recurring (see [../kernel/processes.md](../kernel/processes.md)).
- `fork` copies the parent's fd and socket tables (`VFS_FORK_DUP`, `NET_FORK_DUP`), taking a reference on every object; if the child never gets enqueued (task table full), the tables it took are torn down again rather than leaked.
- `execve` runs the close-on-exec sweeps (`VFS_EXEC_CLOEXEC`, `NET_EXEC_CLOEXEC`) **after** de-threading the group, as Linux's `de_thread` precedes `do_close_on_exec`: running them first let a still-live tokio worker read `EBADF` from its own self-pipe and panic.
- Objects are reference-counted per **open file description** ([`servers/vfs/src/ofd.rs`](../../servers/vfs/src/ofd.rs)), so a pipe end, a socket or an epoll registration dies with its *last* reference wherever that reference lives — another fd, a forked child, or an SCM_RIGHTS message in flight.

## Limitations and open work

- The private IPC syscalls (`SYS_IPC_SEND` to any port id), `SYS_SPAWN` and the debug serial syscall carry no credential checks. See [security.md](security.md#open-gaps).
- Several tables remain fixed and are worth watching: the VFS `LOCKS` table (64), evdev clients (16, LRU eviction), `EXIT_LOG` (256 entries, scanned under the run-queue lock), the pipe counter.
- `servers/proc` is dead code and could be deleted.

## Code and tests

- Port dispatch: [`ipc/src/port.rs`](../../ipc/src/port.rs) (`send`, `register_handler`).
- Server wiring and boot registration: [`kernel/src/init.rs`](../../kernel/src/init.rs).
- Syscall → server translation and teardown: [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs).
- Tests that exercise the lifecycle: `userland/killmt` (multithreaded kill), `userland/forktest`, `userland/exectest` (`exec_hides_cloexec_from_siblings`), `userland/scmtest` (in-flight SCM_RIGHTS), `userland/vfstest` (`fd_layout`), `userland/fdmaxtest`. See [../development/testing.md](../development/testing.md).

## Pages in this section

- [vfs.md](vfs.md) — fd tables, path resolution, mounts, tmpfs, shared memory, pipes, `/proc`, synthetic `/sys`, open-file pools, permission enforcement in resolution.
- [filesystems.md](filesystems.md) — the f2fs server, the xattr/ACL contract crate, the block-device layer (loop, GPT, BLKPG), `/dev/root` and USB root.
- [networking.md](networking.md) — smoltcp TCP/IP with DHCP and ICMP, and AF_UNIX in full.
- [tty-and-vt.md](tty-and-vt.md) — the tty server, line discipline, job control, PTYs, virtual terminals and VT switching.
- [security.md](security.md) — accounts, `/etc/shadow`, login, credentials, permission enforcement, and the privilege gaps that remain.

## See also

- [../kernel/ipc.md](../kernel/ipc.md) — ports, messages and reply ports.
- [../kernel/syscalls.md](../kernel/syscalls.md) — the Linux syscall personality that fronts these servers.
- [../kernel/event-loop.md](../kernel/event-loop.md) — how `VFS_POLL`/`NET_POLL` readiness feeds poll/select/epoll.
