# Kernel Wave K2 — Event-Loop Layer, Design

Scope: make epoll/poll/select actually *block* on a waitqueue instead of yield-spinning;
add signalfd4, inotify stubs; fix /proc/self/exe. Consumers: tokio/mio (working via
busy-poll today), calloop/Smithay (M4-M5), and a compositor + D-Bus + a dozen clients.

Read-only survey; no code changed. Line refs are post-K1-commit approximate — re-locate by symbol.

---

## 0. Ground truth from the survey

**Blocking primitive that already exists and works** (`sched/src/lib.rs:756-791`,
`runqueue.rs:153-181`): three-phase `block_on_port`:
1. `block_on_port_prepare(port)` — mark current task `Blocked`, `blocked_on = Some(port)`,
   *while still executing* (safe: syscalls run IF=0, `on_cpu` claim held).
2. Caller re-checks its condition. A producer that fires after the caller's last check
   already sees `Blocked` → its `unblock_port(port)` flips it `Ready`. Lost-wake closed.
3. `block_on_port_commit()` = `yield_now()`. If a wake raced in, task is already `Ready`.

`unblock_port(port)` (`lib.rs:1126`) scans the run queue, flips every task
`blocked_on == Some(port)` from `Blocked→Ready`, then `wake_up_an_idle_cpu()`.
`deliver_signal` (`lib.rs:319-340`) *also* flips any `Blocked` task `Ready` (this is the
existing EINTR wake). **This is the exact model K2 reuses.**

**Why today burns CPU**: `sys_epoll_wait` / `sys_poll` / `sys_ppoll` / `sys_select` never
call `block_on_*`. They loop `probe → irq_window() → yield_now()`. The task stays
`Running`/`Ready`, so the scheduler re-dispatches it immediately → one vCPU pegged per
blocked poller (exactly the x86_64 tokio "307% CPU" symptom seen in S1, though that one
had a second deadlock cause).

**Readiness is queried live**, not cached: `probe_fd_events_seq(pid,fd,events)`
(`syscall.rs:6010`) routes fd→owning server: `vfs::handle(VFS_POLL)` or
`net::handle(NET_POLL)`, plus fd 0-2 and console-proxy carve-outs. Both `handle_poll`s
already return **(revents, seq)** — a monotonic per-object event counter used for EPOLLET
emulation. VFS seq sources today: pipe (`PipeRing.seq`), eventfd (`EVENTFD_SEQ`), timerfd
(cumulative expirations). **Net sockets return no seq (`None` → level-only).**

**The 82d0cc3-class hazard is live**: `sys_epoll_wait` holds `EPOLL_INSTANCES.lock()`
across the whole probe loop → `probe_fd_events_seq` → `vfs::handle`/`net::handle`, and
writes user memory (`core::ptr::write` to `events_ptr`) *under the lock*
(`syscall.rs:5914-5944`). `poll/ppoll/select` don't hold an epoll lock but *do* raw-read/
write user memory directly (`syscall.rs:2106-2114`, `6149-6161`) inside the loop — those
are pre-faulted-ish but still raw. K2 must remove all server-calls and user-mem access
from under any spinlock.

**Tick machinery**: `sched::timer_tick_irq` (`lib.rs:1007`) runs at 100 Hz on the BSP,
bumps `TIMER_TICKS`, and calls a **single** `TICK_HOOK` fn pointer (`lib.rs:998-1005`,
contract: try_lock only, non-blocking). **Audio already owns that one hook slot.**
`ticks()` = 100 Hz counter; all poll timeouts are computed in tick units.

**Caps today**: epoll `MAX_EPOLL_INSTANCES=16`, `MAX_EPOLL_INTERESTS=32`,
`MAX_EPOLL_FDS=32`. Net `MAX_SOCKS=16`, `MAX_CONNS=32`, `MAX_PROCS=64`, `RING_SIZE=4096`
(K1-C raises the socket side).

**/proc/self/exe**: hardcoded `→ "/bin/init"` for *every* process at
`syscall.rs:4916`. No per-process exe path is stored anywhere; `Task` has no `comm`/`exe`
field. VFS `gen_proc_self_content` (`vfs:2128`) hardcodes comm/cmdline `"leandros"`.

**signalfd4 / inotify**: both `→ log_enosys` (`syscall.rs:1146`, `1240`). Syscall number
constants already defined for both arches.

---

## 1. Waitqueue architecture

### Decision: ONE global poll wait-channel, reused by poll/ppoll/select/epoll_wait; edge wakes from the servers; a deadline check on the tick.

Rationale for a **single global channel** over per-object waitqueues: the workload is
~15-30 tasks (compositor + busd + a dozen clients). A per-object `wait_queue_head` model
(register/deregister each waiter on each interested pipe/socket/eventfd, teardown on close)
is the "correct" Linux design but adds large surface through net_server and vfs plus
lifetime hazards, to avoid a thundering herd we don't have. A global wake of a few dozen
tasks that each re-probe and re-block is cheap and — critically — reuses the *proven*
three-phase `block_on_port` protocol verbatim. This also **unifies** poll and epoll onto
one mechanism (see §2), which the brief asked for.

### Who owns the queue

`sched` owns it. Add (all in `sched/src/lib.rs`):

```
const POLL_WAIT_CHANNEL: u32 = 0xFFFF_FF01;   // reserved; port::alloc never returns this

pub fn block_on_poll_prepare()  { block_on_port_prepare(POLL_WAIT_CHANNEL) }
pub fn block_on_poll_cancel()   { block_on_port_cancel() }
pub fn block_on_poll_commit()   { block_on_port_commit() }
pub fn wake_poll()              { unblock_port(POLL_WAIT_CHANNEL); }  // wake_up_an_idle_cpu inside
```

Reusing `blocked_on` with a sentinel needs zero new scheduler fields and inherits the
signal-EINTR wake for free (deliver_signal flips any `Blocked`→`Ready`). Sentinel is
outside the real port id range so `unblock_port` from a genuine IPC never touches pollers
and vice-versa. (Alternative if overloading `blocked_on` is distasteful: a dedicated
`blocked_on_poll: bool` + a parallel wake scan — ~15 more lines, no behavioral change.)

### The wait loop (identical shape for all four syscalls)

```
compute (infinite, deadline)                       // as today
loop {
    // ---- PROBE: no epoll/server lock held across user-mem writes ----
    snapshot interests (epoll: copy the interest array out under EPOLL_INSTANCES.lock,
                        then DROP the lock)
    for each fd in snapshot: (revents, seq) = probe_fd_events_seq(...)   // lock-free now
    apply EPOLLET/ONESHOT gating; write revents to user memory (no lock held)
    if nready > 0 { commit last_seq updates under a brief EPOLL_INSTANCES.lock; return }
    if timeout==0 || (!infinite && ticks() >= deadline) { return 0 }
    if interrupted() { return -EINTR }

    // ---- BLOCK: three-phase, closes lost-wake ----
    sched::block_on_poll_prepare();                 // state = Blocked
    if !infinite { sched::register_poll_deadline(deadline); }   // §Timeout
    re-probe cheaply: if anything now ready OR interrupted { block_on_poll_cancel(); continue }
    sched::block_on_poll_commit();                  // yield; woken by wake_poll / signal / tick
}
```

The re-probe between `prepare` and `commit` is the whole correctness argument: any edge
landing after the first probe but before we sleep either shows up in the re-probe or lands
as a `wake_poll` against an already-`Blocked` task. Both are safe because syscall context
runs IF=0, so a same-CPU tick cannot interpose; a cross-CPU wake just flips us `Ready`.

### How readiness edges are published (who calls `wake_poll`)

Every not-ready→ready transition of a pollable object calls `sched::wake_poll()` **after
releasing that object's server lock** (never under UNIX_CONNS / PIPE_RINGS / FD_TABLES):

| Source | Site (`file:function`) | Edge |
|---|---|---|
| AF_UNIX data in | `net:handle_send` / `handle_sendmsg`, after `ring.write` | peer POLLIN |
| AF_UNIX space freed | `net:handle_recv` / `handle_recvmsg`, after `ring.read` | writer POLLOUT |
| AF_UNIX connect/accept | `net:handle_connect` / `handle_accept` | listener POLLIN / peer up |
| AF_UNIX close | `net` socket-close path | peer POLLIN\|POLLHUP |
| pipe write | `vfs` pipe write path (`put` loop) | reader POLLIN — **also bump `PipeRing.seq`** |
| pipe read | `vfs` pipe read path (`get` loop) | writer POLLOUT |
| pipe EOF/EPIPE | `vfs:pipe_drop_ref` (already bumps seq) | POLLHUP/POLLERR |
| eventfd write | `vfs` eventfd write (already bumps `EVENTFD_SEQ`) | POLLIN |
| timerfd expiry | tick deadline check (§Timeout) — no syscall drives it | POLLIN |
| console/serial/evdev input | input IRQ enqueue: `evdev_server` key-push, arch serial RX | fd0 POLLIN |
| signal delivered | `sched:deliver_signal` / `deliver_signal_process` | signalfd POLLIN |

**Gap to fix**: pipe *data* writes do **not** currently bump `PipeRing.seq` (only
reader/writer-count changes do, in `pipe_drop_ref`). For correct EPOLLET on pipes and for
the wake itself, bump seq + `wake_poll` on every write and read. (mio uses eventfd, not a
self-pipe, so this hasn't bitten yet, but tokio's signal self-pipe and calloop's ping
source depend on it.)

### Timeout integration (true ~0% idle for infinite waiters)

Two wake classes, so an infinite-timeout waiter with no timer gets **zero periodic
wakeups** (the M1 exit criterion):

- **Edge wakes** (table above) cover all data-driven readiness. An idle tokio runtime
  parked in `epoll_wait(-1)` on its eventfd waker + quiet sockets sleeps until a real
  event — 0% CPU.
- **Deadline wakes** for finite timeouts and timerfds. Add
  `NEXT_POLL_DEADLINE: AtomicU64` (u64::MAX = none, in tick units). Contributors:
  - a finite-timeout poll/select/epoll_wait publishes its deadline via
    `fetch_min` when it blocks (`register_poll_deadline`);
  - `vfs::earliest_timerfd_deadline()` (new; scans `TIMERFD_POOL`, includes interval
    auto-reload for periodic timers) — folded into the tick check so a periodic timerfd
    nobody re-arms still fires.
  - Generalize the single `TICK_HOOK` into `TICK_HOOKS: [usize; 4]` (audio keeps its
    slot). Register a K2 poll-deadline hook from kernel init:
    ```
    fn poll_tick() {
        let due = min(NEXT_POLL_DEADLINE.load(), vfs::earliest_timerfd_deadline());
        if ticks() >= due { NEXT_POLL_DEADLINE.store(MAX); wake_poll(); }
    }
    ```
    Try_lock only (tick contract); a contended tick just defers the wake ≤10 ms, which is
    already the timeout granularity. Woken timed-waiters that re-block re-publish their
    deadline via `fetch_min`.

This costs a timerfd-pool scan (O(16)) per tick only, and a `wake_poll` only when a
deadline actually passes — not every tick.

**Bring-up safety net** (behind a cfg flag, OFF for the idle test): a 10 Hz unconditional
`wake_poll` while any poller is blocked. If an edge site is missed, waiters still make
progress (10 ms-100 ms late) instead of hanging, which makes a missing-edge bug a
performance blip rather than a lockup during development. The M1 ~0% idle test runs with it
OFF to prove edge coverage is complete.

---

## 2. poll/select consistency — unify, don't parallel

**Judgment: unify.** poll/ppoll/select today busy-loop on the same `probe_fd_events`
helper that epoll uses; the only thing they lack is the block. Route all four through the
single `block_on_poll_*` + `wake_poll` facility and the shared probe helpers. There is no
reason for a second mechanism, and a second one would double the edge-site audit surface.
Concretely: `sys_poll`, `sys_ppoll`, `sys_select` each replace their
`irq_window(); yield_now()` tail with the block sequence in §1 (prepare / re-probe /
publish-deadline / commit). `probe_fd_events` (level, no seq) stays their probe; epoll uses
`probe_fd_events_seq`. Same wait channel, same wakers.

---

## 3. EPOLLET / EPOLLONESHOT semantics

**Keep the existing seq-based edge emulation; extend seq to AF_UNIX sockets; add ONESHOT.**

- **Level interests** (no EPOLLET) — calloop's default and most Smithay sources: fire
  whenever `cur != 0`. Unchanged.
- **EPOLLET interests** — mio/tokio register `EPOLLIN|EPOLLOUT|EPOLLET|EPOLLRDHUP`: fire
  only when the object's seq advanced past `interest.last_seq`. Unchanged mechanism, but:
  - **Add a per-`UnixConn` combined event seq** (bumped on data-in / space-freed /
    peer-close). Today net returns `None` → level, and `probe_fd_events_seq` force-routes
    all `fd >= SOCK_FD_BASE` to the level path (`syscall.rs:6024`). Under the *new blocking*
    model that is wrong: an EPOLLET socket that is level-writable would re-fire on every
    single `epoll_wait` return → tokio spins forever re-checking a POLLOUT that never
    "edges." Fix: net `handle_poll` returns a seq in reply `data[8..16]` (mirroring vfs),
    and drop the socket carve-out in `probe_fd_events_seq`. A single combined per-conn seq
    is sufficient for tokio/mio because they register both directions on one interest and
    re-derive per-direction readiness after each wake; the occasional cross-direction
    spurious wake is harmless (tokio just finds nothing new and re-blocks).
  - fd 0-2 stay `None`/level (correct — they are effectively always level).
- **EPOLLONESHOT** (cheap, include): after an interest fires, if
  `events & EPOLLONESHOT`, disarm it (store `events = 0` / an `armed=false` flag) until an
  `epoll_ctl(MOD)` re-arms. ~3 lines in the fire path + the ctl path. tokio doesn't use it;
  some libraries (and a future threadpool) do.

Net EPOLLET correctness is the single most important behavioral change here: without it,
"real blocking" would convert tokio's socket handling from busy-but-working into
busy-and-still-working (POLLOUT storm), defeating the idle goal.

---

## 4. signalfd4 (minimal) + inotify stubs

### signalfd4 — for calloop's `Signals` source

New VFS pseudo-fd `VnodeKind::SignalFd { mask: u64 }` (mirrors EventFd/TimerFd in the vfs
pool). New opcode `VFS_SIGNALFD_CREATE`; `syscall.rs` `SIGNALFD4 => sys_signalfd4`.
- create: `signalfd4(-1, &mask, flags)` reads the 8-byte sigset → allocate vnode storing
  mask; `fd != -1` updates the mask on the existing vnode.
- poll: `POLLIN` iff `(sched::pending_signals() | sched::shared_pending_signals()) & mask
  != 0`. seq = a signal-delivery counter (so an EPOLLET signalfd works; calloop uses level
  so either is fine).
- read: for each pending signal in mask, dequeue it
  (`sched::clear_pending_signal`) and emit a 128-byte `struct signalfd_siginfo` with
  `ssi_signo` set, rest zeroed. Return 128×n, or -EAGAIN (NONBLOCK) / block on empty.
- wake: `deliver_signal` / `deliver_signal_process` call `wake_poll()`.

**The lie to document**: only `ssi_signo` is populated (calloop reads only that);
`ssi_code/ssi_pid/ssi_uid/…` are zero. Reading via signalfd consumes the pending bit
(POSIX-correct: the signal is *accepted*, not run through a handler) — this relies on the
caller having blocked the signal in all threads (calloop does), which our masked-delivery
path (`deliver_signal_process` parks a fully-masked process-directed signal on the leader's
`shared_signal_pending`) already honors.

### inotify — keep cosmic-settings-daemon's config watch alive

New `VnodeKind::Inotify` pseudo-fd; opcodes `VFS_INOTIFY_CREATE`. `syscall.rs`
`INOTIFY_INIT1/ADD_WATCH/RM_WATCH => sys_inotify_*`.
- `inotify_init1(flags)` → real fd, Inotify vnode.
- `inotify_add_watch(fd, path, mask)` → fake watch descriptor from a monotonic per-vnode
  counter (≥1). Optionally stat `path` and return -ENOENT if genuinely absent (polite;
  settings-daemon tolerates either).
- `inotify_rm_watch` → 0.
- poll: always `(0, 0)` — **never fires**. read: -EAGAIN (NONBLOCK) — never yields events.

**The lie to document**: watches are accepted and silently never fire; there is no live
config reload. This is exactly the plan's intent (§K2): "a valid fd that never fires." The
fd is a legitimate epoll interest that simply sits quiet among the daemon's other sources.

---

## 5. /proc fix spec

**Root cause**: `syscall.rs:4916` — `if path == b"/proc/self/exe" { target = b"/bin/init" }`.
A literal, never keyed to the actual executed binary. No exe path is stored per process
(the `Task` struct has no `comm`/`exe` field; execve at `syscall.rs:2660` resolves
`kpath` — the absolute path — but discards it after loading).

**Fix** (store once, read in two places):

1. `sched`: add a tgid-keyed side table (avoids churning `Task`'s raw-copy layout used by
   fork/clone):
   ```
   static EXE_PATHS: Mutex<[(Pid, [u8; 256], u16); N]>;
   pub fn set_exe_path(tgid, &[u8]); pub fn exe_path(tgid, &mut [u8]) -> Option<usize>;
   pub fn clear_exe_path(tgid);
   ```
   - `sys_execve`: `sched::set_exe_path(current_tgid(), kpath.bytes())` on success.
   - init/PID1 spawn: seed `"/bin/init"` so the current answer stays correct for PID1.
   - fork/clone (`fork_current`, `clone_thread` non-thread case): child (new tgid) inherits
     a copy of the parent's exe path until it execs; CLONE_THREAD siblings share via tgid.
   - process (leader) exit: `clear_exe_path(tgid)`.
2. `sys_readlinkat` `/proc/self/exe` (and, cheap bonus, `/proc/<pid>/exe`): return
   `sched::exe_path(tgid)`; fall back to `"/bin/init"` only if unset.
3. `vfs::gen_proc_self_content`: derive **comm** (basename of exe path) and **cmdline**
   from `sched::exe_path` instead of the hardcoded `"leandros"`. SHOULD, not MUST.

**Other /proc entries consumers actually touch** (surveyed against real usage, not guessed):

| Path | Consumer | Status |
|---|---|---|
| `/proc/self/exe` | Rust std `env::current_exe`, backtrace symbolication, some COSMIC resource-path lookups | **MUST fix** (this task) |
| `/proc/self/fd/N` | std fd-close fallback, `ttyname` | Present, resolves via `VFS_FD_PATH`; and `close_range` is wired so std doesn't fall back here. OK |
| `/proc/self/maps` | backtrace/unwind | Returns empty; tolerated. OK |
| `/proc/self/cmdline`, `/stat`, `/status` | logging crates, `comm` readers | Present; comm hardcoded. SHOULD fix comm |
| `/proc/cpuinfo`, `/proc/stat` | `available_parallelism` | Uses `sched_getaffinity` (wired); cpuinfo present. OK |
| `/proc/sys/{kernel/*,vm/overcommit_memory}` | musl/malloc probes | Present. OK |

**tokio, calloop, zbus need no /proc beyond `self/exe`**: tokio = eventfd/epoll/timerfd,
no /proc; calloop = epoll/timerfd/signalfd, no /proc; zbus = UDS + SO_PEERCRED, no /proc.
`task_struct comm` is nice-to-have, not required by any fatal-at-spawn component.

---

## 6. Lock-order analysis vs the K1 order

K1 order (from K1 notes): `FD_TABLES → TMP_FILES → TMP_VMOS` before AS-busy; net
`UNIX_CONNS`. K2 introduces `EPOLL_INSTANCES`, `EPOLL_FDS` (kernel), and uses `RUN_QUEUE`
(via `wake_poll`/`block_on_poll`), plus new vfs pseudo-fd pools (`SignalFd`, `Inotify`) and
a per-`UnixConn` seq (no new lock — same `UNIX_CONNS`).

Rules that keep K2 clean and fix the 82d0cc3 class:
- **`EPOLL_INSTANCES` becomes a leaf lock.** Never held across `vfs::handle`,
  `net::handle`, or any user-memory access. epoll_wait *snapshots* the interest array under
  it, drops it, then probes and writes user memory lock-free, then re-takes it only to
  write back `last_seq` (matching by fd, since a sibling thread's `epoll_ctl` may have
  mutated the slot). This directly removes the "holds EPOLL_INSTANCES across
  probe_fd_events_seq → vfs/net" hazard flagged in the S1 notes. Never nested with
  FD_TABLES/UNIX_CONNS/AS.
- **`wake_poll` (→ `RUN_QUEUE`) is always called with no server lock held.** Every edge
  site drops `UNIX_CONNS` / `PIPE_RINGS` / `EVENTFD_*` / `FD_TABLES` *before* calling
  `wake_poll`. So the acquire order is `<server lock>` … release … `RUN_QUEUE`, never
  nested. This preserves the K1 rule that RUN_QUEUE (and by extension any user-mem-touching
  work) is never taken under a server spinlock.
- **`block_on_poll_prepare/commit` touch only `RUN_QUEUE`** and no user memory — same as
  the proven `block_on_port`. No violation of "no user memory under RUN_QUEUE / IRQ-off
  spinlock."
- **IRQ-context `wake_poll` (tick hook)** takes `RUN_QUEUE` from the BSP timer IRQ. Safe:
  task-context `RUN_QUEUE` holders run IF=0, so the tick cannot preempt a same-CPU holder;
  cross-CPU is a plain spin. This is the same shape as the existing tick-driven SIGALRM/
  itimer delivery (`set_real_itimer` → signal). The tick's `vfs::earliest_timerfd_deadline`
  uses **try_lock** on `TIMERFD_POOL` (tick contract) and skips on contention.
- New vfs pseudo-fd pools (`SignalFd`, `Inotify`) sit at the same level as
  `EVENTFD_*`/`TIMERFD_POOL` under the existing vfs lock discipline — no new ordering.

No inversion introduced.

---

## 7. Cap changes

| Cap | Now | K2 | Note |
|---|---|---|---|
| `MAX_EPOLL_INSTANCES` | 16 | **64** | one per {comp, busd, each client, tokio rt} |
| `MAX_EPOLL_INTERESTS` | 32 | **512** | compositor watches every client fd + input + timers |
| `MAX_EPOLL_FDS` | 32 | **128** | dup aliases; ≥ instances |
| net `MAX_SOCKS`/`MAX_CONNS` | 16/32 | (K1-C) | K2 relies on K1-C raising these |

**Memory flag (riskiest cap cost)**: a flat `[EpollInstance; 64]` with
`interests: [EpollInterest; 512]` at ~32 B/interest = **~1 MiB static BSS**. Options:
(a) accept it (simplest; kernel has the RAM) — recommended for K2; (b) a shared global
interest pool (e.g. 4096 entries on a free-list, instances hold index ranges) so a few
instances can burst to 512 while most hold a handful — ~40 extra lines, saves ~900 KiB.
Recommend (a) now, note (b) as the follow-up if BSS budget tightens.

---

## 8. Touch list (`file:function`)

**`kernel/src/syscall.rs`**
- `sys_epoll_wait` (~5871): rewrite — snapshot interests / drop lock / probe lock-free /
  EPOLLET+ONESHOT gate / write user mem / re-lock only for `last_seq` write-back / three-
  phase `block_on_poll`. (Biggest single change.)
- `sys_poll` (2088), `sys_ppoll` (2135), `sys_select` (6118): replace `yield_now` tail with
  shared `block_on_poll` sequence + `register_poll_deadline`.
- `probe_fd_events_seq` (6010): drop the `fd >= SOCK_FD_BASE → None` carve-out; take net's
  seq from the NET_POLL reply.
- consts (5652, 5653, 5695): cap bumps.
- dispatch: `SIGNALFD4` (1146) → `sys_signalfd4`; `INOTIFY_*` (1240) → `sys_inotify_*`.
- `sys_readlinkat` `/proc/self/exe` (4916): use `sched::exe_path`.
- `sys_execve` (2660): `sched::set_exe_path` on success.
- new thin wrappers: `sys_signalfd4`, `sys_inotify_init1/add_watch/rm_watch` → vfs opcodes.

**`sched/src/lib.rs`**
- `POLL_WAIT_CHANNEL`, `wake_poll`, `block_on_poll_{prepare,cancel,commit}`,
  `register_poll_deadline`, `NEXT_POLL_DEADLINE`.
- `EXE_PATHS` table + `set/exe/clear_exe_path`; hook fork/clone/exit.
- `TICK_HOOK` → `TICK_HOOKS: [usize; 4]`; kernel registers `poll_tick`.
- `deliver_signal` / `deliver_signal_process`: add `wake_poll()`.

**`servers/net/src/lib.rs`**
- `UnixConn`: add `seq: u64`.
- `handle_send`/`handle_sendmsg`, `handle_recv`/`handle_recvmsg`, `handle_connect`/
  `handle_accept`, close path: bump seq + `sched::wake_poll()` after dropping `UNIX_CONNS`.
- `handle_poll` + `poll_reply`: return seq in `data[8..16]`.

**`servers/vfs/src/lib.rs`**
- pipe write/read paths: bump `PipeRing.seq` + `wake_poll` (write→reader, read→writer).
- eventfd write: `wake_poll` (seq already bumped).
- `earliest_timerfd_deadline()` (new, scans `TIMERFD_POOL` incl. interval reload);
  `handle_timerfd_settime`: publish deadline / `wake_poll` on immediate arm.
- `VnodeKind::SignalFd`, `VnodeKind::Inotify` + create/poll/read/close handlers + opcodes.
- `gen_proc_self_content` (2128): comm/cmdline from `sched::exe_path`.

**input IRQ path**: `evdev_server` key-push + arch serial-RX handler: `sched::wake_poll()`
on enqueue.

---

## 9. Riskiest spots

1. **Missing an edge site → permanent hang** for an infinite-timeout waiter (the deadline
   backstop only covers timed waits). The edge audit in §1 must be exhaustive. Mitigation:
   the cfg-gated 10 Hz safety wake during bring-up turns a missed edge into a latency blip;
   turn it off for the idle test to prove completeness.
2. **Net EPOLLET seq coverage** (§3): missing any of {data-in, space-freed, close} bump
   points makes tokio either stall (edge never re-fires) or storm (POLLOUT never edges).
3. **1 MiB BSS** from 64×512 interests (§7).
4. **Periodic timerfd republish**: `earliest_timerfd_deadline` must account for
   interval-reloaded next-expiry, or a calloop periodic timer that the app doesn't re-arm
   stops firing.
5. **IRQ-context `wake_poll` lock order** vs the existing tick-driven itimer/SIGALRM path —
   verify no new RUN_QUEUE inversion (analysis in §6 says clean; confirm at implementation).
6. **signalfd consume-vs-deliver**: reading signalfd must dequeue the pending bit so the
   signal isn't *also* run through a handler; get the interaction with
   `deliver_signal_process`'s leader-parking right.
7. **Pipe seq not bumped on data write today** — a latent EPOLLET-on-pipe bug that this
   wave both exposes (real blocking) and must fix.

**Estimated diff**: ~600-750 lines across 4 files (epoll_wait rewrite ~120; poll/ppoll/
select ~60; sched block+wake+deadline+exe ~150; net seq+wakes ~80; vfs pipe/eventfd/
signalfd/inotify/proc ~250; tick+timerfd ~50; caps+dispatch ~30). Medium.

---

## 10. Test plan

**M1 idle-CPU exit criterion (measurable, in QEMU):**
- New `userland/tokioidle`: a multi-thread tokio runtime spawns N (~16) tasks each awaiting
  a quiet UDS/mpsc, plus 2 low-rate timers; prints `IDLE_START`, calls
  `getrusage(RUSAGE_SELF)`, sleeps 5 s of wall time on one timer, calls getrusage again,
  prints `stime+utime` delta and `IDLE_PASS`/`IDLE_FAIL` (threshold e.g. <100 ms consumed
  over 5 s). This is the guest-side, tooling-free check: the busy-poll version burns
  seconds of stime; the blocked version ≈0.
- Host-side cross-check via the `run-leandros` driver: sample QEMU host CPU% over the 5 s
  idle window; PASS if <~5% per vCPU (busy version pegs a vCPU). Use a persistent serial
  reader (known QEMU capture gotcha) so the long idle window isn't mistaken for a hang.
- Run with the 10 Hz safety-net wake **OFF**, so passing proves edge coverage is complete.

**Regression:**
- `userland/polltest` (exists, passes today) — run unchanged both arches; proves poll/
  select still correct after the block conversion.
- `tokio-echo-selftest` (S1, pass=3/skip=1 both arches) — proves sockets+timers+mpsc under
  real blocking (EPOLLET socket seq path).
- New `epolltest` (or extend polltest): for each of {pipe, eventfd, UDS, timerfd}: (a) an
  EPOLLET interest fires exactly once per edge (writer bumps, reader drains, no re-fire
  until next write); (b) a level interest re-fires while ready; (c) `epoll_wait(timeout)`
  returns 0 within ~one tick of the deadline; (d) a "woke exactly once" counter proves it
  blocked rather than spun.
- signalfd: block SIGUSR1 in all threads, create signalfd, `raise(SIGUSR1)`, `epoll_wait`,
  read → assert `ssi_signo == SIGUSR1`.
- inotify: `inotify_init1` ≥0, `add_watch` ≥1, epoll on it → times out (never fires).
- `/proc/self/exe`: a test binary asserts `readlink("/proc/self/exe")` == its own exec path
  (not `/bin/init`).
- All of the above on **both** x86_64 and aarch64 via `run-qemu.sh` (release only).
