# The event-loop layer

`poll`, `select` and `epoll` are what every event loop in the desktop sits in — tokio, calloop, mio, the `polling` crate, Wayland, D-Bus — so this layer decides both correctness (readiness that is real, edges that are never lost) and idleness (a parked loop must cost nothing). Readiness is always computed by the object that owns the fd; waiters park on one global poll wait-channel; servers publish readiness *edges* onto it with targeted, hash-tagged wakes; and every deadline is serviced by a one-shot timer. This page also covers the fd types that exist mainly to feed event loops: eventfd, timerfd, signalfd and inotify.

## Overview

| Piece | Where |
|---|---|
| `poll`/`ppoll`/`select`/`pselect6`, `epoll_create1`/`ctl`/`wait`/`pwait`/`pwait2`, readiness routing, deadline service | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| `POLL_WAIT_CHANNEL`, tags, `block_on_poll_*`, `wake_poll*`, `request_poll_wake`, deadline hint | [sched/src/lib.rs](../../sched/src/lib.rs) |
| VFS readiness (`VFS_POLL`): pipes, eventfd, timerfd, signalfd, inotify, ptys, DRM, evdev, VT | [servers/vfs/src/lib.rs](../../servers/vfs/src/lib.rs) |
| Open file descriptions (epoll item lifetime) | [servers/vfs/src/ofd.rs](../../servers/vfs/src/ofd.rs) |
| Socket readiness (`NET_POLL`): AF_UNIX, inet, listeners | [servers/net/src/lib.rs](../../servers/net/src/lib.rs) |
| Original design record | [../design/k2-epoll-design.md](../design/k2-epoll-design.md) |

**Who implements what.** The kernel implements `poll`, `ppoll`, `select`, `pselect6` and the `epoll` family natively; musl-linked programs (the whole desktop) call them directly. relibc implements `poll` and `select` in userspace on top of `epoll_create1`/`epoll_ctl`/`epoll_pwait`; its `poll` registers each distinct fd once with the union of its entries' events, fans results out to duplicates, treats `EPERM` from `epoll_ctl(ADD)` (a regular file, a directory) as "always ready", and maps `EBADF` to `POLLNVAL` — Linux `poll` semantics, which an earlier version that returned -1 on any `ADD` error did not have (see [../userland/libc.md](../userland/libc.md)).

**Handles.** A descriptor is translated to what it names before routing: a VFS object is probed by its own number, a socket by its net-server handle (`0x0100_0000` + slot), an epoll fd by its epoll handle (`EPOLL_FD_BASE` = `0x0200_0000` + index). Readiness routing (`poll_fd_state`, `probe_fd_events_seq_inner`) resolves in the *probed* process's fd table (`vfs::resolve_fd`) and dispatches on the handle range; an interest on a socket or epoll fd stores the handle.

## How it works

### Real readiness, never fabricated

Each probe asks the object that owns the fd and masks the answer against the requested events (`POLLERR`/`POLLHUP`/`POLLNVAL` are always reported, as Linux):

- **VFS** (`VFS_POLL`): pipe ring occupancy and per-end reference counts, eventfd counter, timerfd expiry, signalfd pending set, pty buffers, DRM events, evdev queues, VT change edges.
- **Net** (`NET_POLL`): socket peer occupancy and liveness, listener backlog, shutdown state, `SO_ERROR`.
- **Console stdio**: fd 0 is readable when synthetic console input (a cursor-position reply), a key event or serial data is pending; fd 1/2 are always writable. A pty or `/dev/tty0` on 0/1/2 falls through to `VFS_POLL` instead — a shell under a terminal emulator would otherwise have polled the UART.
- **Nested epoll fds** are readable exactly when their own interest list has a ready event (never writable), recursing at most `EPOLL_MAX_NEST` = 4 levels like Linux's `EP_MAX_NESTS`; at the limit they report not-ready, which can delay an event but never spin.

The pre-Phase-9 code reported whatever the caller *asked* for, unconditionally. Pipe write ends follow Linux's real `POLLOUT` contract: writable only when at least `PIPE_BUF` (4096) bytes are free, measured against a stock Linux pipe (free = 4095 not writable, 4096 writable). Pipe ends are reference-counted: an end held by several fds (via `dup`/`dup2` or inherited across `fork`) signals EOF/`POLLHUP`/`EPIPE` only when the *last* fd on that end closes, so shell pipelines stay correct across `fork`.

### The wait loop

`poll_wait`, `select_wait` and `epoll_wait_until` share one shape:

1. **Probe** every interest. For epoll, each interest is copied out of `EPOLL_INSTANCES` under the lock and the lock dropped *before* the probe, which calls into vfs/net and must never run under a spinlock; results are written to user memory lock-free, then `last_seq`/`ONESHOT` state is committed under a brief re-lock, matched by `(fd, ofd)` since a sibling's `epoll_ctl` may have changed the slot meanwhile. The loop walks only up to the instance's high-water mark `hi`.
2. Return on events, on deadline, or on a deliverable signal (`EINTR`, or a restart for poll/select — see [signals.md](signals.md#syscall-restart)).
3. **Park** three-phase on `POLL_WAIT_CHANNEL`: `block_on_poll_prepare_masked(deadline, mask)` publishes Blocked plus the deadline plus the interest mask in one `RUN_QUEUE` hold; a read-only re-probe (and an `EPOLL_CTL_GEN` check) catches an edge that landed between the probe and the prepare; then commit. The re-probe is what closes the check-then-sleep lost wake.

A woken waiter simply loops: re-probe, and either return or re-park. Every blocking primitive in the kernel has that re-check-after-yield contract, which is what makes a spurious wake always safe — and what lets [job-control stops](signals.md#a-stopped-thread-loses-channel-wakes-on-purpose) drop wakes deliberately.

### Readiness edges and targeted wakes

Producers publish edges with `wake_poll_tagged(tag)`, where `tag = poll_tag(class, index)` hashes the object identity — `PIPE`, `PTY`, `EVENTFD`, `TIMERFD`, `UNIX`/`UNIX_RD`/`UNIX_WR` (per connection end), `INET`, `CONSOLE`, `DEVVT`, `DRM`, `EVDEV`, `WAIT` (a parent in `wait4`, by tgid), `PIDFD` (a process exit, by tgid), `INOTIFY`, `EPOLL` (an instance itself) — to a single bit of a 64-bit mask with a splitmix64 finaliser. A parked poller's `poll_mask` is the OR of its interests' tags, and `unblock_port_tagged` skips any parked task whose mask does not intersect the wake's tag. A hash collision only wakes an extra poller, which re-probes and re-parks; it can never suppress a needed wake.

**Exact keys on top of the bloom.** Sixty-four bits saturate: a tokio reactor watching AF_UNIX sockets for `EPOLLIN|EPOLLOUT` contributes three tags per socket, so busd with ~45 clients set ~60 of the 64 bits and was woken by nearly every AF_UNIX, pipe and eventfd wake in the system — ten to eighteen tasks per send during video playback, where one was interested. `poll_key(class, index)` is the full 64-bit splitmix64 value whose low six bits are the old tag bit. The objects that are woken by key — connected AF_UNIX data edges (`unix_rd_key`/`unix_wr_key`), pipes (`pipe_key`), eventfds (`eventfd_key`), timerfds, inet sockets (`inet_key`, the net daemon's per-pass readiness wake), the net daemon's own kick (`NETD_KEY`), GPU syncobjs (`POLL_KEY_SYNCOBJ`) and evdev device nodes (`evdev_poll_key`, forwarded by the VFS for the `DynamicDevice` node) — report the same keys in their `VFS_POLL`/`NET_POLL` probe replies (`data[33]` = `0x4b`, count in `data[34]`, keys in `data[40..64]`), and wake with `wake_poll_key`/`try_wake_poll_key`. A waiter's `PollSet` holds the bloom mask, `poll_kbits` (bits that only keyed interests explain) and up to `POLL_FP_MAX` = 512 16-bit fingerprints; a keyed wake whose bit is in `kbits` reaches the task only on a fingerprint match. Refinement is conservative: an unkeyed contribution to a bit, fingerprint overflow, `widen_poll_masks` and legacy parks are never refined, and bit-only wakes ignore refinement.

**Foreign tags** (`POLL_REPLY_FOREIGN` = `0x4c`) keep objects that are never targeted by a keyed wake — signalfd, ptys, pidfds, inotify, `/dev/tty0`, device-server fds, AF_UNIX listeners, an epoll instance's own tag — from poisoning that refinement: their bits join the mask without counting as unkeyed (`PollSet::add_foreign_bits`). This is sound because a foreign object is only ever woken bit-only or by broadcast. The kinds are whitelisted; anything new stays unrefined. On the laptop this cut "unrefined" keyed wakes from 16.6 k/s to 4.7 k/s during YouTube playback and the desktop's total CPU from 527 % to 375 %.

**Input wakes by device, not the channel.** An input event wakes, at the end of the drain, only the waiters on the device it arrived on (`evdev_server::wake_dev`, keyed, one per device with pending events), the broadcast-mask console readers the console tap feeds, and — for a key routed to a text VT — that session's pty tag (`tty_server::vt::kbd_wake_tags`). Until lane herd1 it was a broadcast: every input event woke every poller and epoll waiter in the system. `evtest2 herd` (driven by `run-leandros/evherdtest.py`, in `regress`) parks on the keyboard while the tablet moves and reads its own wake count from `/proc/sysstat` (`w` lines, column 8 `pwakes`; futex waiters append `fut= fval= rip= rsp=`).

**Signals are tagged, not broadcast.** A signal post wakes `signal_wake_tag(tgid)` = `poll_tag(SIGNALFD, tgid)`; `sigsuspend`/`sigtimedwait` park with that tag, signalfd probes report it (for the *caller's* tgid, since the answer reads the caller's pending set), and the thread a signal is queued on is still made Ready directly. Before, every post was a broadcast to ~180 parked tasks on the desktop.

**Deferred wakes keep their keys.** `request_poll_wake_key` stores exact keys in a 32-slot lock-free table (falling back to the bit when full); pipe/eventfd fallbacks, syncobj completions, the net daemon's IRQ kick and the timerfd tick use it. The deadline tick's timerfd-expiry wake is keyed too (`WakeKeys`, at most 16 per pass, else bit-only). Wake scans filter parked slots through a mirror in the run queue (`pk_port`, `pk_mask`, `pk_deadline`) instead of dereferencing every boxed `Task`.

The safety rule, because a lost wake is a hang: `POLL_TAG_ALL` (all ones) is the broadcast. Every producer not converted to a tag, and every interest that cannot be hashed, **must** fall back to `POLL_TAG_ALL`, never to something narrower; a broadcast wake reaches every non-zero mask, and a broadcast mask is reached by every wake, so a forgotten site degrades to the old herd, never to a missed wake. A park whose computed mask is 0 (an empty nested epoll) parks with `POLL_TAG_ALL` instead, since mask 0 would be immune even to broadcasts. Parks that wait only for a deadline (`nanosleep`, kernel watchdog threads) use `POLL_MASK_TIMED_ONLY` = 0, reached only by broadcasts and the deadline service; `sigsuspend` and `sigtimedwait` park with the thread group's signal tag. They used to register `POLL_TAG_ALL` and were woken by every targeted wake in the system — about 55 wakes per second for one idle `sleep 60` on the greeter.

`epoll_ctl` wakes the instance's own parked waiters (tag `EPOLL` + slot) only when the newly armed fd is already ready; otherwise it *widens* their park masks with the new fd's tag (`widen_poll_masks`) so that fd's later wake reaches them, which can only add wakes, never lose one. Widening applies to one sleep only; it never widens a ready-list listener's filter (see below).

**Level changes wake now; edge-only changes are deferred and coalesced.** A pipe write that makes the ring non-empty, or a read that crosses the `PIPE_BUF` free-space line, flips a readiness *level*, and a poller parked with that level false has no other way to learn it — so those wake immediately. A write or read that only advanced the object's edge `seq` matters solely to an `EPOLLET` interest parked with `last_seq == seq`; those call `request_poll_wake_tagged`, which ORs the tag into `POLL_WAKE_PENDING` and is paid by the next tick (`service_deferred_poll_wake`), bounding the delay at ~10 ms while collapsing an unbounded burst of writes into at most 100 wakes per second. The swap happens *before* the wake, so a request published while a wake is in flight re-arms for the next tick instead of being cleared; a contended `try_wake_poll` re-arms too.

There is no periodic safety wake (`POLL_SAFETY_WAKE = false`): `idletest` proves edge coverage is complete by checking that an idle system takes zero wakes per tick.

### The epoll ready list

Like Linux's `ep_poll_callback` model, an `epoll_wait` re-probes only what a wake can concern, instead of every registration after every wake (lane epollready, 2026-10-17). It is on by default; `epoll.ready=0` on the kernel command line (`EPOLL_READY`) selects the old full re-probe loop, which remains the reference implementation.

- **Poll-wake listener** (`sched::PollListen` in `Task::listen`, with a `listening` bitmap and `ls_mask` mirror in the run queue). From a thread's first `epoll_wait` on an instance until it parks on something else, every wake scan (`unblock_port_tagged_inner`, the keyed deadline service, `widen_port_masks` for a *parked* listener) records each wake that passes the thread's filter — the same mask/kbits/fingerprint rule as a parked poller — into a per-task `WakeRing` (8 exact keys; overflow and bit-only wakes OR into a tag word; a deadline flag). It records whether the thread is parked, running, in user space or blocked elsewhere. `poll_listen_park` parks only if the ring is empty, in the same `RUN_QUEUE` hold. `poll()`/`select()` and legacy parks end the listener, because they reuse the fingerprints the filter shares.
- **Per-thread snapshot** (`EpCache`, kernel/src/syscall.rs, stored in `Task::ep_cache`): per registration its fd, description, ctl sequence (`EpollInterest::mseq`), armed state, last probe tag and whether it was ready. Valid while the same instance (`EpollInstance::id`) and the same listener are in use.
- **`epoll_wait_ready`**, per pass: diff the instance against the snapshot (a new or changed fd/description, or an `ADD`/`MOD`, makes a candidate); candidates are then everything if the wake reason is unknown, otherwise unknown-tag, broadcast-tag and nested entries, last call's ready entries, and entries a recorded key or bit names (`ep_hit`). Only candidates are probed, with the old loop's exact fire/`EPOLLET`/`ONESHOT` rules. If any entry's tag changed, the filter is rebuilt and installed, and entries whose tag changed under a narrow filter are re-probed once more under it (after two such rechecks, under a broadcast filter). A real sleep that ends with an empty ring means an unknown reason, so a full re-probe follows.
- A cache miss (first call, another instance, a `poll()` in between) starts a record-everything listener *before* its full pass, so no second pass is needed.

Invariants, because a lost wake is a hang: while a listener is on, every matching wake is in its ring; an entry is probed under a filter that covered it, re-probed after the new filter is installed, or probed under broadcast; events committed in a pass are always returned; and **a listener's filter is never widened by `epoll_ctl`** — a new or re-armed registration reaches it through the per-pass diff (a `MOD` of an entry the current filter does not hold is re-probed as new). An earlier cut widened listener filters, and one `ADD` of a broadcast-tagged fd anywhere (relibc's `poll` `ADD`s every fd on every call) turned colliding listeners into broadcast for good; the desktop A/B caught it, and `epolltest foreign_add_does_not_widen` guards it.

The ready list depends on every readiness change being announced by a wake whose tag or key matches the object's probe — full re-probes no longer rescue a silent producer. Every pollable class was audited for this; the evdev VT-switch resync (now a broadcast at the end of `complete_switch`), pty `TCFLSH`/`TCSETSF` freeing queue space, and signalfd's tag were fixed on the way.

Measured on a COSMIC + Firefox session (aarch64/HVF, 12 s): kernel CPU in syscalls 8.7 s → 4.2 s, `epoll_pwait` 256 → 100 µs per call, busd's kernel CPU 923 → 61 ms. `epolltest bench` (41 fds, one ready) goes from ~51 µs to ~9.6 µs of kernel time per round trip on aarch64. `/proc/wakestat` reports `# epoll calls N miss N probes N unknown N parks N`.

### Deadlines

A timed waiter's deadline lives in its own `Task::poll_deadline` (the authority) and is folded into the lock-free `NEXT_POLL_DEADLINE` hint, and `register_poll_deadline` arms the current CPU's one-shot timer for it. `poll_deadline_service` — from the one-shot interrupt or the 100 Hz tick — folds expired timerfds into single expiry edges (disarming one-shots, advancing periodics), wakes waiters whose deadline is due or whose mask names an expired timerfd, and recomputes the hint exactly under the same `RUN_QUEUE` hold. Folding matters: an armed one-shot timerfd whose owner was briefly descheduled used to sit with a past deadline, fire "due" every tick and mass-wake the system, starving the owner so it never consumed the expiry — the idle-desktop wake storm that kept `ext-idle-notify`'s `Idled` from ever firing. Details of the timer side are in [time.md](time.md#the-tick-and-one-shot-deadlines).

### epoll semantics

- **Instances and limits**: the instance table, each instance's interest list and the epoll fd (handle) table are heap vectors that grow with use (ceilings 1 Mi instances, 1 Mi interests per instance, 16 Mi handles). Until lane fdmax they were fixed at 1024 / 512 / 2048 (~20 MB of `.bss`).
- **Ownership is the thread group**: an epoll fd is usable from every thread of the process, plus its dups and fork copies. A per-task check once made crossterm's lazily created, shared event reader unusable from every thread but the one that created it — `EBADF`, which crossterm retries forever. The owner tgid is cached at create; if the creator leaves first, ownership is re-homed.
- **Items are keyed by `(fd, open file description)`**, as Linux keys them by `(file, fd)`. An item lives until the description's last reference anywhere is gone — not until the fd is closed — via the `ofd` table's release hook (`epoll_ofd_released`). If the owner's fd no longer names the description, the probe finds a dup, a fork child or an `SCM_RIGHTS` receiver that still holds it (`vfs::ofd_holder`/`net_server::ofd_holder`) and caches where. A description held only by a *queued* `SCM_RIGHTS` message reports through `inflight_vfs`. `POLLNVAL` is never reported by epoll; a reused fd number gets its own item.
- **Level vs edge**: level interests fire whenever a ready bit is set. `EPOLLET` interests fire only when the object's per-event `seq` has advanced since the last delivery (`last_seq`, `u64::MAX` = never delivered, so the first readiness always fires). That is what stops a permanently level-ready fd — a pipe at EOF, mio's never-drained eventfd waker, a level-writable socket — from pinning a tokio reactor in a zero-timeout spin, while never dropping a genuine edge. Objects with no edge source (console fds) report `seq = None` and stay level even under `EPOLLET`. Sockets have edge sources: connected TCP sockets bump their `seq` whenever the net daemon sees a socket's queue lengths or readiness change across an `Interface::poll`; UDP and ICMP sockets on every datagram received or sent (smoltcp's per-socket wakers) and on a `can_send` flip; listeners once per connection that becomes established. Until lane sshspin TCP sockets were level under `EPOLLET`, and an idle SSH session pinned tokio's sshd reactor at 100 % of a CPU on the permanently writable socket (`epolltest tcp_et_no_refire`, `udptest`).
- **`EPOLLONESHOT`**: an `armed` flag cleared on fire and restored by `epoll_ctl(MOD)`.
- **`epoll_ctl` errors follow Linux `do_epoll_ctl`'s order**: `EFAULT`, `EBADF` (either fd), `EPERM` (target not pollable: regular files, directories, `/dev/null`, `/dev/zero`, `/dev/fb0`, block devices — procfs/sysfs snapshots, dmabufs and memfds deliberately stay pollable because smithay polls dmabufs), `EINVAL` (not an epoll fd, or the instance itself), then `EEXIST`/`ENOENT`. A full interest table is `ENOSPC`. `EPERM` is checked up front only for `ADD`; `MOD`/`DEL` check it only when no item was found, so calloop's hot `MOD` re-arm path pays nothing extra. The first 32 refusals are logged as `[EPOLL] ctl err=N op= fd= tgid= <exe>`.
- **Lifetime**: epoll fds are shared across `fork` (each holder with its own `FD_CLOEXEC`), honour `EPOLL_CLOEXEC`/`F_DUPFD_CLOEXEC`/`F_SETFD`, are closed by `execve`'s close-on-exec sweep and by `close_range` (which also implements `CLOSE_RANGE_CLOEXEC`), and an instance is freed when its last number goes. Socket calls on an epoll fd return `ENOTSOCK`, not `EBADF`.
- **`struct epoll_event` layout** matches Linux exactly: 12-byte `#[repr(C, packed)]` with the data word at offset 4 on x86-64 only (glibc's `EPOLL_PACKED`), the natural 16-byte layout with data at offset 8 on AArch64. The kernel uses arch-conditional offsets with `read_unaligned`/`write_unaligned`, so libc and kernel agree on both.

### poll and select

`poll_wait`/`select_wait` probe each fd through the same `probe_fd_events` and park through the same `poll_block`. Negative `pollfd.fd` entries are skipped with `revents = 0`; `poll(NULL, 0, ms)` is a real sleep. `ppoll` and `pselect6` honour their signal mask (installed for the wait, restored on return, with the sigsuspend mechanism when a handler runs; `sigsetsize` must be 8), and `ppoll`/`pselect6`/`select` write the remaining time back, except for a zero timeout. With nothing ready, a deliverable signal wins over an expired deadline, as in Linux's `do_poll`/`do_select`. Timeouts: `NULL` blocks forever, `{0,0}` checks once.

### eventfd, timerfd, signalfd, inotify

All four are VFS objects in global, refcounted pools (refcounted so a `dup`'d fd cannot alias another fd's slot after a close — that aliasing once killed cosmic-comp's calloop when a ping eventfd's drain emptied an unrelated source's counter). All honour `O_NONBLOCK` and `*_CLOEXEC` at creation.

- **eventfd** (`MAX_EVENTFDS` = 1024): a counter plus a per-slot `seq` bumped on every write, which is what lets mio's never-drained `EPOLLET` waker re-fire once per `wake()`. `EFD_SEMAPHORE` is not implemented.
- **timerfd** (256 slots): see [time.md](time.md#timerfd).
- **signalfd**: reports `POLLIN` while a masked signal is pending (thread or process level); a read dequeues each pending masked signal as a 128-byte `signalfd_siginfo` with `ssi_signo`, `ssi_code`, `ssi_pid`, `ssi_uid` and `ssi_status` taken from the same `SigInfo` a handler would get, so reaping children through a signalfd and through a `SIGCHLD` handler see the same death. `ufd == -1` creates; otherwise the named signalfd's mask is replaced.
- **inotify**: a valid fd that accepts watches (returning fake watch descriptors ≥ 1) and never delivers events. It exists to keep config-watch sources (cosmic-settings-daemon, `notify::Watcher`) quiet in an event loop; there is no live reload. `IN_CLOEXEC` matters: dropping it leaked every config-watch inotify fd across `execve` and filled late children's fd tables (the cosmic-greeter `EMFILE` crash loop).

Dropping `O_NONBLOCK` on any of these is not a cosmetic bug: a read that would return `EAGAIN` on a fd the kernel thinks is blocking yield-spins in the kernel's generic blocking read loop, pinning the caller's thread there instead of letting its event loop poll other fds.

Pool sizes are session-wide budgets, and every one of them was raised after a measured exhaustion: each event loop (tokio, calloop, `polling`) allocates an eventfd waker and often a timerfd, a full COSMIC session runs about 20 processes with several loops each, and one `cosmic-term` adds about 17 eventfds (tokio + calloop + wgpu). At 256 eventfds the second terminal died at startup on calloop's `Ping` ("TooManyOpenFiles") with no leak anywhere.

## Design decisions

### One global wait-channel, made cheap, instead of per-object wait queues

**Choice.** `poll`, `select` and `epoll_wait` (and generic timed parks: `wait4`, `nanosleep`, the net daemon's cadence) share one `POLL_WAIT_CHANNEL`, and every woken poller re-probes its interest set.

**Why.** It reuses the proven three-phase `block_on_port` protocol verbatim, and for a workload of a few hundred tasks a wake that each parked poller re-checks is cheaper than the register/deregister bookkeeping a per-fd wait-queue model needs on every `epoll_ctl` and every park. The sentinel channel id is outside the real port range, so IPC wakes never touch pollers and poll wakes never touch IPC waiters.

**The per-instance wait-channel plan was refuted by measurement — do not build it.** When the COSMIC desktop managed 0.20 fps on x86_64/KVM, 74% of running guest CPU was in the kernel, and the obvious culprit was the thundering herd: `wake_poll` made every parked poller Ready. After the two real costs were removed — the epoll probe loops walked all 512 interest slots taking the global `EPOLL_INSTANCES` lock once per slot (22.3% of guest CPU, fixed with the per-instance high-water mark `hi`), and `tgid_of` took `RUN_QUEUE` and scanned every task on each `epoll_wait` (10.6%) — `unblock_port` measured 0.7% and the nested readiness probe 0.7%. The herd's cost had never been the wakeups; it was the 512-slot rescan each woken task did. Per-instance channels would have touched every readiness publisher in vfs and net and risked exactly the lost-wake races the design avoids, for about 1%. The fix took the desktop from 0.20 to 1.00 fps (the content ceiling of an idle desktop with a 1 Hz clock) and idle headroom from 22% to 41% (`f7c4577`, `eb9422e`). Coalescing evdev's per-event wakes ~8.75× later made the input path no faster either — the same result in the input-storm regime.

**What did change: targeted wakes on the same channel.** The herd did become the problem in a different regime. With tokio pools four wide on four vCPUs, cosmic-comp's unbuffered stderr — thousands of tiny pipe writes, one per tracing fragment — woke every parked poller in the system per write, each re-probing under the global `FD_TABLES`/`PIPE_RINGS` locks; cosmic-comp's handshake with cosmic-session took a constant 40.2 s on `-smp 4` versus 1.6 s on `-smp 1`. The decisive discriminator was redirecting the compositor's stdout to a file (2.9 s), which isolated the child-stdio pipe path. Two changes fixed it while keeping one channel: edge gating (wake only on a level change, defer seq-only changes to the tick), and hash-tagged targeted wakes (`daaf2cc`), 40.2 s → 1.6 s. This does not contradict the refutation above: per-object filtering became worth it only once the woken set was large *and* the wake rate was thousands per second; it was done as a 64-bit mask on the existing channel, not as per-object queues.

### Edge-triggered epoll on per-object sequence numbers

**Choice.** `EPOLLET` compares a per-object `seq` to the interest's `last_seq`, instead of tracking readiness transitions.

**Why.** Several runtimes register fds edge-triggered and never drain them (mio's eventfd waker), and some objects are permanently level-ready (a pipe at EOF). A transition model either misses re-arms (a second `wake()` on an already-readable eventfd is no level transition) or spins. A sequence number bumped on every write fires exactly once per producer event. The earlier code force-edged every VFS fd regardless of what was registered; that was wrong for calloop, whose interests are level by default.

### Linux epoll item lifetime (open file descriptions)

**Choice.** An open-file-description table (16384 entries, `gen << 16 | idx` ids) in the VFS, referenced by every fd and socket entry; epoll items key on it.

**Why.** Number-keyed items broke the moment a description outlived its number: a registration made before `fork`, `dup` or `SCM_RIGHTS` either vanished with the original fd or reported a stale `POLLNVAL` forever, and with fd-number reuse a new fd inherited someone else's registration. Before the change, `polltest`'s Linux-derived expectations failed four ways on the base kernel (fork `0 1 0 0 0 0` instead of `1 2 0 0 1 0`, SCM `0 0` instead of `n1=1 n2=0`, and both `close_range` cases). Each new expectation was first run on a Linux host, and both architectures now print the Linux numbers exactly.

### Blocking, not spinning

`ppoll`, `epoll_wait`, `select`, `wait4`/`waitid`, `nanosleep` and the net daemon all park rather than yield-spin, so an idle system is genuinely idle (`48e38c6`, `2c924ea`). Before that, init's getty-loop `usleep(1 s)` alone kept PID 1 at 100% CPU churning the scheduler run loop. `idletest` guards it.

## Limitations and open work

- **`epoll_pwait`'s signal-mask argument is ignored** (it is routed to the same body as `epoll_wait`), and so is `epoll_pwait2`'s. On AArch64 there is no `epoll_wait` syscall, so every epoll wait there passes through `epoll_pwait`.
- **`ELOOP` for epoll cycles and the `EPOLLEXCLUSIVE` `EINVAL` rules** are not implemented (cycles are bounded by the nesting depth instead).
- **`CLOSE_RANGE_UNSHARE`** is a no-op: exact for a single-threaded caller, but in a multithreaded caller the siblings see the closes.
- inotify never delivers events.
- `EFD_SEMAPHORE` is not implemented.
- A queued `SCM_RIGHTS` *socket* (as opposed to a VFS description) reports nothing until received; its probe contributes the broadcast tag meanwhile, so no wake is lost.
- Pipes have a 16 KiB ring (Linux: 64 KiB).
- relibc's `poll()` builds a fresh epoll instance per call, so it always misses the ready-list cache (it still gets the in-call savings).

## Code and tests

| What | Where |
|---|---|
| Wait loops, `poll_block`, `probe_fd_events`, `poll_fd_state`, `epoll_*`, `poll_deadline_service`, `close_range` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| Poll channel, tags, deferred wakes, deadline hint | [sched/src/lib.rs](../../sched/src/lib.rs) |
| Ready list: `epoll_wait_ready`, `EpCache`, `EPOLL_READY` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| `poll_key`, `PollSet`, `PollListen`, `WakeRing` | [sched/src/lib.rs](../../sched/src/lib.rs) |
| Wake attribution census (`/proc/wakestat`, outcome table with `sched.wakestat=1`) | [sched/src/gdwake.rs](../../sched/src/gdwake.rs), [sched/src/idlestat.rs](../../sched/src/idlestat.rs) |
| Pipes, eventfd, timerfd, signalfd, inotify, `handle_poll`, `fd_epoll_eperm` | [servers/vfs/src/lib.rs](../../servers/vfs/src/lib.rs) |
| Open file descriptions | [servers/vfs/src/ofd.rs](../../servers/vfs/src/ofd.rs) |
| Socket readiness, in-flight `SCM_RIGHTS` | [servers/net/src/lib.rs](../../servers/net/src/lib.rs) |

Regression suites: `polltest` (real fd readiness for `poll`/`select`/`epoll`, `epoll_wait` timeout, pipe `POLLHUP` writer refcount across `dup`, `PIPE_BUF` `POLLOUT` rule, Linux `poll` semantics for regular files/duplicates/negative and closed fds, `epoll_ctl` errnos, fork sharing and last-reference, `close_range`, in-flight SCM, `poll_timeout_wake_latency`); `epolltest` (level re-fire, `EPOLLONESHOT`, signalfd, inotify, nested epoll, TCP `EPOLLET` no-refire, wake isolation — `unrelated_unix_traffic_no_wake`, `timerfd_set_not_woken_by_unix_traffic`, `own_timerfd_among_many_wakes`, `foreign_add_does_not_widen` — and ready-list cases: LT stays reported, ET once, keyed and unkeyed bursts, ring overflow, nested warm, DEL while parked, ONESHOT re-arm; run it with and without `epoll.ready=0`; `epolltest bench` prints kernel ns per round trip and per spurious push); `udptest` (`EPOLLET` edges of UDP, ICMP and TCP listeners); `wakepolltest` (same-thread `EPOLLET`/level eventfd re-arm, pipe and AF_UNIX blocking wakes, `FUTEX_WAIT_BITSET` deadlines); `smpwaketest` (pipe-`EPOLLET` under a poller herd, forked-child stdout); `idletest`; `scmtest` (`send_on_epoll_enotsock`, fd passing). Ctrl-T prints the epoll census (`[EPOLL] … ofds=live/16384 forkcopies=`).

## See also

- [time.md](time.md) — clocks, one-shot deadlines, timerfd
- [signals.md](signals.md) — how signals wake parked pollers and restart poll/select
- [threads-and-futex.md](threads-and-futex.md) — the other wait primitive
- [scheduler.md](scheduler.md) — `RUN_QUEUE`, `block_on_port`, idle
- [../servers/vfs.md](../servers/vfs.md) — fd tables, pipes, open file descriptions
- [../servers/networking.md](../servers/networking.md) — AF_UNIX readiness and `SCM_RIGHTS`
- [../design/k2-epoll-design.md](../design/k2-epoll-design.md) — the original event-loop design
