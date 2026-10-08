# IPC

Ports are the kernel's message-passing primitive: bounded FIFO queues with a single owner, addressed by a 32-bit id. The kernel exposes `send` (non-blocking enqueue), `recv` (blocking dequeue on a port you own) and `call` (send, then block on a private reply port), and the server layer is built on ports whose *handlers* run synchronously in the sender's context. This page also covers the `LIVE_BUCKETS` limit that once masqueraded as out-of-memory, and how tasks discover server ports through the auxiliary vector.

## Overview

| Item | Value | Where |
|---|---|---|
| Port id namespace | 1..`MAX_PORTS` = 65536 | [ipc/src/port.rs](../../ipc/src/port.rs) |
| Ports live at once | `LIVE_BUCKETS` = 512 | `port.rs` |
| Queue | `QUEUE_DEPTH` = 16 slots (15 usable: one slot distinguishes full from empty) | `port.rs` |
| Message | `tag: u64`, `reply_port: u32`, 440-byte inline `data`, `has_cap`/`cap` words — 472 bytes | [ipc/src/message.rs](../../ipc/src/message.rs) |
| Ownership | Only the owner may `recv`; anyone holding the id may `send` | `recv_as` |
| Syscalls | `SYS_IPC_SEND` 511, `SYS_IPC_RECV` 512, `SYS_IPC_CALL` 513, `SYS_PORT_CREATE` 514 | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |

There is no shared memory between tasks unless explicitly mapped (`MAP_SHARED`, memfd, DRM buffers — [memory.md](memory.md)).

## How it works

### Ports

`port::create(owner_pid)` allocates an id and a bucket in the port table, an open-addressed hash table of `LIVE_BUCKETS` buckets keyed by `id % LIVE_BUCKETS`, with linear probing bounded at 16 steps and tombstones on close so later entries in a probe chain stay reachable. Each bucket holds the owner pid, a 16-slot ring of `Option<Message>`, and an optional direct handler. The table is one `TrackedMutex` (`PORT_TABLE`), visible to the watchdog by name ([scheduler.md](scheduler.md#per-cpu-tick-watchdog)).

`port::send` enqueues and then wakes tasks blocked on that port (`sched::unblock_port`) — **unless the owner is the sending task itself**. Only the owner can receive, and it is running, so nobody can be parked on it; that wake was a full run-queue scan that found nothing, ~170 k times a second on an idle desktop, and the single largest `RUN_QUEUE` holder (lane runq). A full queue returns `QueueFull` → `EAGAIN`; an unknown port returns `EBADF` to userspace.

`recv` uses the three-phase block ([scheduler.md](scheduler.md#blocking-the-three-phase-protocol)): publish `Blocked` on the port, *then* look at the queue, then yield. The older check-then-block order lost the wake whenever the send landed between the empty check and the park, leaving a message queued while the receiver slept forever. A port closed while its owner waits wakes it with `EBADF`.

When a task exits, the task-exit hook `release_by_owner(pid)` tombstones every port it owns and wakes their waiters, in batches of 64 under the lock with the wakes outside it, looping until a batch comes back short. (It used to tombstone the surplus beyond one batch without ever waking its waiters.)

### `call` and reply ports

`sys_call(port, msg)` is the send-and-wait idiom:

1. Take the caller's **reply port**, creating it lazily on first use and caching it in `Task::reply_port` (also published per CPU as `CURRENT_REPLY_PORT`, so the lookup is two relaxed loads).
2. Stamp it into `msg.reply_port` and send.
3. `recv` on the reply port, which the caller owns.

A server replies by sending to `msg.reply_port`. `reply_port` defaults to `u32::MAX` in `Message::empty()`, so a message that was never stamped never triggers a reply — a guard against accidental reply loops. (The earlier design blocked the caller on the *server's* port, which it does not own; that is where the ownership check's `EACCES` came from.)

### Direct handlers: servers run in the caller's context

A port can carry a registered handler (`port::register_handler`). Sending to such a port does not enqueue anything: `send` drops the table lock, calls `handler(&msg, caller_pid, port)` **on the sender's own kernel stack**, and delivers the returned message to `msg.reply_port`. The VFS, f2fs, DRM, evdev and PipeWire servers register handlers this way ([servers/README.md](../servers/README.md#a-server-is-a-port-with-a-direct-handler)).

The kernel-side client is `servers::vfs::call_port` ([servers/vfs/src/lib.rs](../../servers/vfs/src/lib.rs)), used for every operation on a mounted filesystem. Because a direct handler has already replied by the time `send` returns, `call_port` first tries `recv_as` on the reply port and takes the reply with **zero** `RUN_QUEUE` holds; only if nothing is there does it fall back to the prepare/recv/commit park. A failed `port::create` returns `ENOMEM` (never a zeroed "success" reply, which callers used to decode as an empty file) and prints `[VFS] ENOMEM: no reply port for this task` once.

So the server layer is architecturally a set of port endpoints — each owns its state and is reached only by message — but in the current build the endpoints are crates linked into the kernel and the "message" is a function call. Why that trade was made, and what it costs (deep kernel stacks, the rule that server code never touches user memory under its locks), is discussed in [servers/README.md](../servers/README.md#synchronous-in-caller-dispatch-rather-than-server-threads) and [invariants.md](invariants.md).

### `Channel` and the capability slot

`ipc::Channel` pairs a client and a server port for a bidirectional rendezvous, closing the first if the second cannot be created; nothing in the tree uses it today. `Message` reserves a capability word (`has_cap`/`cap`) for passing a shared-memory object by capability; no server sets it — large payloads travel by pointer instead (servers run in the caller's address space; see the `KPath` note in [syscalls.md](syscalls.md#one-path-resolver)) or by real shared mappings.

### Auxv service discovery

Rather than a name-service daemon, `execve` stamps server port ids into the ELF auxiliary vector of every new image, so a program finds its servers in O(1) without a round trip. The private tags, all above `AT_MINSIGSTKSZ`:

| Tag | Value | Contents |
|---|---|---|
| `AT_LEANDROS_VFS_PORT` | 256 | VFS server port |
| `AT_LEANDROS_NET_PORT` | 257 | net server port (currently `u32::MAX`: no code registers it; sockets are reached through syscalls) |
| `AT_LEANDROS_AUDIO_PORT` | 258 | PipeWire server port |

The `leandros-libc` start-up code ([userland/libc/src/start.rs](../../userland/libc/src/start.rs)) reads them into `get_vfs_port()`, `get_net_port()`, `get_audio_port()`; [servers/libc-shim](../../servers/libc-shim/src/lib.rs) does the same for its consumers. There is no DRM tag: DRM is reached through `/dev/dri/*` file descriptors and ioctls like on Linux, and the older documentation's `get_drm_port()` does not exist. musl and relibc ignore unknown tags, so stock Linux binaries are unaffected.

The full auxv `execve` builds (18 pairs): `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_RANDOM` (16 bytes on the stack), `AT_PAGESZ`, `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID`, the three Leandros ports, `AT_ENTRY`, `AT_BASE` (interpreter base or 0), `AT_SECURE`, `AT_HWCAP`, `AT_EXECFN`, `AT_NULL`. `prctl(PR_GET_AUXV)` returns `EINVAL` rather than an empty vector: returning 0 made rustix read a page size of 0 and `bottom` report 0 bytes of memory. See [processes.md](processes.md#execve) for the stack layout.

## Design decisions

### `LIVE_BUCKETS`: the ceiling that looked like RAM

`MAX_PORTS` (65536) bounds only the **id namespace**; `LIVE_BUCKETS` bounds how many ports can **exist at once**, and it is the real system-wide ceiling. Every task that reaches a server through `call_port` holds one bucket as its reply port from its first call until it exits, and `call_port` is on the path of every `open`, `stat`, `getdents64` and the `execve` that opens the binary. The consumers are **threads**, not processes.

It was 64. On 2026-08-08 a stock COSMIC session on x86-64/KVM was measured at **61/64** live ports when the desktop settled and 64/64 once four more components started, with 1.1–1.2 GiB of RAM free. Unblocking four autostarted multithreaded components (a D-Bus `ServiceUnknown` fix in `busd`) pushed it over during start-up: `stat("/usr/share/X11/xkb")` — a directory that exists — returned errno 12, `xkb_context_new` returned `NULL`, SCTK dereferenced it at offset `0x880`, and the session wedged. The recorded cause had been "a failed `calloc`"; nothing had allocated. Two investigations had gone looking for a memory leak. Fix: 512 buckets (6× the measured peak of 84; ~7.5 KiB per bucket, ~3.8 MiB of BSS). Falsified by mutation in both directions on both architectures: back to 64 reproduced the whole chain down to the same faulting address (`CR2=0x880` on x86-64, `FAR=0x880` on AArch64, a struct offset and so architecture-independent); 24 buckets made `ls` of an existing directory fail with "Out of memory" after 20 background `sleep`s.

What stays from that investigation:

- **`report_table_full` is not gated.** When `port::create` fails it prints, once per boot, `[IPC] port table FULL: N/512 live buckets -- … this is ipc::port::LIVE_BUCKETS, not RAM`. Two lines, only when it matters.
- **`PORT_STATS`** (committed `false`) prints one line per new occupancy high-water mark, at most `LIVE_BUCKETS` lines per boot, because the mark only rises. The occupancy scan is behind the flag, not merely its print, because `create` is a hot path.
- The general rule — an errno must name its cause — is in [memory.md](memory.md#enomem-must-name-its-cause) and [invariants.md](invariants.md#an-errno-names-its-cause-and-a-full-table-says-so).

### A small fixed table, not a dynamic one

The table is a static array because the IPC path must not allocate under its own lock, and a bucket embeds its queue so `send` never allocates either. The price is a hard ceiling, which is acceptable only because exceeding it is now loud. A 16-deep queue of 440-byte messages makes a bucket ~7.5 KiB, which is why the namespace (65536) and the live table (512) are decoupled: a table of 65536 full buckets would be hundreds of MiB of BSS.

### Auxv instead of a name service

A name service would be one more server to start before anything else can find anything, and one more round trip on every program start. The kernel already knows the ports when it builds the image, and auxv is the standard place for "facts the kernel tells a new image"; unknown tags are ignored by every libc, so it costs nothing for stock binaries.

## Limitations and open work

- The table is global and fixed; a process can exhaust it (each thread that touches a mounted filesystem holds a reply port until it exits).
- `AT_LEANDROS_NET_PORT` is never populated; `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID` are always 0 and `AT_HWCAP` is 0 (so AArch64 userspace selects baseline code paths by HWCAP), and `AT_SECURE` is 0 even across a privilege change.
- The capability slot in `Message` and `Channel` are unused; the `capability` crate is not linked ([README.md](README.md#crate-map)).
- The comment above `ipc::init()` in `kernel/src/init.rs` still describes the table as 64 buckets.

## Code and tests

| What | Where |
|---|---|
| Ports, the table, `release_by_owner`, `report_table_full` | [ipc/src/port.rs](../../ipc/src/port.rs) |
| Message format | [ipc/src/message.rs](../../ipc/src/message.rs) |
| `Channel` | [ipc/src/channel.rs](../../ipc/src/channel.rs) |
| `sys_send` / `sys_recv` / `sys_call`, auxv construction | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| Kernel-side client `call_port` | [servers/vfs/src/lib.rs](../../servers/vfs/src/lib.rs) |
| Wait channels, `unblock_port`, per-CPU reply port | [sched/src/lib.rs](../../sched/src/lib.rs) |

Tests: **`racetest`** creates ports with `SYS_PORT_CREATE` and drives the raw send/recv blocking path under contention; every suite that touches a mounted filesystem exercises `call_port`. The port-exhaustion reproducer and the mutation runs are recorded in the m18/m19 investigation notes (2026-08-08).

## See also

- [servers/README.md](../servers/README.md) — the server layer built on ports
- [syscalls.md](syscalls.md) — the private syscall numbers
- [scheduler.md](scheduler.md) — wait channels and the blocking protocol
- [processes.md](processes.md) — `execve` and the initial stack
