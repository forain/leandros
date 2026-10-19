# Broadcast-mask (POLL_TAG_ALL) parkers: audit, 2026-10-19

Read-only audit of local main `315cedab`. Nothing was built or run. Line numbers refer to that tree.
Background: `zerocopy-2026-10-19.md` §4 (9.6k of 15.1k wakes/s go to ALL parkers), `sockspin-2026-10-18.md`,
and the udpanytag fix (`udpany_poll_tag`, servers/net/src/lib.rs:2211).

Already in lane/pollnarrow, so excluded: the `/dev/netctl` arm of vfs `fd_wake_tag`, and the console stdin read at
kernel/src/syscall.rs:6399.

## How a task ends up parked with ALL

Only parks on `POLL_WAIT_CHANNEL` matter. IPC-port waiters also carry ALL (`block_on_port`, sched/src/runqueue.rs:566),
but `unblock_port_tagged_inner` only scans tasks parked on the port being woken (runqueue.rs:831), so poll wakes
never reach them. A task gets an ALL mask in four ways:

1. **It calls `block_on_poll_prepare_until`**, which is `masked(deadline, POLL_TAG_ALL)` (sched/src/lib.rs:2666).
2. **It parks with an empty `PollSet`.** `block_on_poll_prepare_set` turns mask 0 into ALL (sched/src/lib.rs:2692). A
   listener's mask 0 is turned into ALL too (runqueue.rs:627, 667).
3. **One interest's probe answers `PollTag::ALL`.** `PollSet::add` ORs all ones into the mask, so a single untagged fd
   makes the whole epoll/poll/select set a broadcast set, and the epoll listener filter as well (sshd's case).
4. **A blocking read/write/send/recv/connect gets EAGAIN** on a kind with no trusted tag. `vfs_block_hint` and
   `net_block_hint` (kernel/src/syscall.rs:10558-10571) return `(ALL, now + 10 ms)`.

Amplifier: when `sys_epoll_ctl` ADDs a not-ready interest, it calls `widen_poll_masks(self_tag, tag.bits)`
(kernel/src/syscall.rs:11504). If that tag is ALL, every task parked with a mask that shares the instance's self-tag
bloom bit becomes an ALL parker for one sleep. relibc's poll() does one ADD per fd per call. This fixes itself once
the source tags are narrow, so it needs no lane of its own.

## Case table

"Producer" is the code that must wake the new narrow tag. The 100 ms net-daemon broadcast (servers/net/src/lib.rs:3017)
fires only while smoltcp reports activity, so no case may rely on it.

### A. Net server poll replies and blocking socket I/O (`data[32] == 0`, so the kernel uses ALL, syscall.rs:13068)

| # | Case | Where | Narrow tag | Producer |
|---|---|---|---|---|
| A1 | TCP listener | net lib.rs:6976-6994, `InetListening` → tag `None` | new `poll_class::LISTEN` key per `lid`, reported foreign or keyed | listen.rs:212: `maintain_one` bumps `l.seq` when a queued connection becomes ready. Have `listen::maintain` (called at net lib.rs:2918 and 3000) push `listen_key(lid)` into the pass's `wake_keys`. Also wake it in `listen::destroy` (listen.rs:118) and when the listener is closed |
| A2 | ICMP / raw ping socket | net lib.rs:7008, `IcmpBound` → `None` | `PollTag::keyed(&[inet_key(sid, h)])`, as used for UDP | already present: `readiness_moved` (lib.rs:3172) pushes `inet_key` for every socket in the set, ICMP included. A recv with MSG_PEEK (`icmp_peeked`, lib.rs:2303) does not change readiness |
| A3 | Unbound AF_INET UDP, unbound ICMP | lib.rs:7017, 7019: `(POLLOUT, Some(0), None)`. Always writable, so it parks only under EPOLLET after the first edge, and then stays an ALL contributor until its first sendto/bind | per-socket state tag, e.g. `poll_tag(INET, ofd)` keyed, foreign | the bind/connect/first-sendto that installs the smoltcp socket wakes that tag (another thread may be parked on it) |
| A4 | Catch-all `_` | lib.rs:7020 `(0, None, None)`: `None`, `Unbound` TCP/AF_UNIX/AF_INET6-DGRAM, `InetBound` (TCP bound but not listening yet) | the same state tag as A3 | `handle_bind`, `handle_listen`, `handle_connect`: any transition out of these states |
| A5 | AF_UNIX listener whose bound slot is already gone | lib.rs:6890 `listen_sock_id == 0 → None` | state tag, or report POLLHUP | the close path (rare, transient) |
| A6 | Blocking recv/send/connect on any inet socket | net `fd_wake_tag` lib.rs:1629-1640 returns `None` for every non-AF_UNIX state. Callers: syscall.rs:6272, 6476, 10512 (connect, 75 s), 10579 (`net_blocking_op`: send/recv/sendto/recvfrom/sendmsg/recvmsg), 13696 (splice out) | `poll_key_bit(inet_key)` for `InetConnected` and the udpany key bits; `listen_key` for listeners; the A3/A4 state tag otherwise | same producers as above (`readiness_moved`, already in place) |

### B. VFS-side tags (`fd_wake_tag` servers/vfs/src/lib.rs:1604; `poll_vnode_k` 9832-10003)

| # | Case | Where | Narrow tag | Producer |
|---|---|---|---|---|
| B1 | Blocking read on a DynamicDevice: DRM card/render (`-11` at servers/drm/src/lib.rs:394) and evdev (`-11` at servers/evdev/src/lib.rs:1167) | `fd_wake_tag` `_ => None` (vfs:1616). Seen in lane-herd1: tgid 36 took 94 stray wakes in one regress run | ask the server: proxy one VFS_POLL (as `poll_vnode_k` already does, vfs:9938-9946) and use `data[24..32]` when `data[32] == 1`. That gives the DRM `drm_event_poll_tag(open_id)` and evdev `poll_key_bit(evdev_poll_key(dev))`. Do it only on the EAGAIN slow path | already present: `drm_event_wake` (drm_device_interface.rs:4600), evdev `wake_dev`. This shares the arm with pollnarrow's netctl change, so generalise it after pollnarrow merges |
| B2 | Blocking signalfd read (EAGAIN at vfs:7275) | `fd_wake_tag` None | `sched::signal_wake_tag(current_tgid)` (sched/src/lib.rs:1472), the tag sigsuspend uses | already present: every signal post wakes `signal_wake_tag(tgid)` (sched/src/lib.rs:1460, 1491, 1654) |
| B3 | Blocking inotify read (EAGAIN forever, vfs:7301-7304) | `fd_wake_tag` None, so it wakes at 100 Hz plus on every wake in the system, forever | `poll_tag(INOTIFY, 0)` with no deadline | none needed (nothing produces events). Signals make the thread Ready directly |
| B4 | `/dev/pipewire` poll | servers/pipewire/src/lib.rs:504 `_ => Message::empty()`, so revents is 0 and `data[32]` is 0, so ALL. Today the sink only writes, so this is latent | a VFS_POLL arm: POLLOUT, plus a constant foreign tag | none (always writable; the write blocks inside the server) |
| B5 | Device server answers an error, or more than one key | vfs:9953 (`raw < 0`), 9971 (`data[34] != 1`) | keep ALL (this is the safety rule), but log it once | n/a |
| B6 | Always-ready kinds: RamFile, TmpFile, MountedFile, null/zero/urandom, DevFb, BlockDev, SysBlock | vfs:9867-9874 → ALL. Ready, so a park happens only under EPOLLET after the edge, or when the requested bits miss POLLIN/POLLOUT. epoll ADD refuses regular files (EPERM, syscall.rs:11425) but not DevFb/BlockDev, and poll/select accept all of them | a constant foreign "static" tag | none (readiness never changes) |
| B7 | `fcntl(F_SETLKW)` / `flock(LOCK_EX)` waits | vfs:8410, 8450 → `block_on_poll_prepare_until(+10 ms)` | `poll_tag(FLOCK, hash(LockKey))`, keeping a 1 s backstop deadline | every LockRecord clear: `release_locks` (vfs:8288; callers 7624, 8129 = close/exit, 8428 = LOCK_UN) and the F_UNLCK arms (8369, 8397, 8440) |

Unreachable arms that stay ALL by design: vfs:9985 (DevVt vt != 0; the kernel answers it first) and vfs:10000
(DevStdio/None/SockAlias/EpollFd).

### C. Kernel park sites and the kernel side of probes

| # | Case | Where | Narrow tag | Producer |
|---|---|---|---|---|
| C1 | Console stdio (fd 0-2, `/dev/tty`, dup'd stdin, `/dev/ttyN`) inside poll/epoll/select | syscall.rs:13028 → `PollTag::ALL` (readiness from `console_input_pending` + `has_key_event(0)` + serial, syscall.rs:12888) | the console-input tag pollnarrow introduces for the 6399 read, plus `evdev_poll_key(0)` as a key | pollnarrow's producer (the evdev/serial drain path) |
| C2 | Empty poll/select sets: `poll(NULL, 0, ms)` used as a sleep, all fds < 0, `select(0, ...)` | `poll_fds_mask` (12785) / `select_fds_mask` (12797) return an empty set, so `poll_block` (12738) parks ALL | `POLL_MASK_TIMED_ONLY` via `block_on_poll_prepare_masked`. Only poll/select: epoll always adds its self tag (11643, 12070) | none (deadline and signals only) |
| C3 | `VT_WAITACTIVE` | servers/tty/src/vt.rs:1110 → `prepare_until(+20 ms)` | `poll_tag(DEVVT, 0)` | already present: the VT switch broadcasts (vt.rs:1060) and wakes DEVVT (1077, 1088, 1298) |
| C4 | Rare kernel fallbacks: nest limit (12770, 13102), `epoll_slot_of` None (12993), holder lookup failing (12305), SCM_RIGHTS in-flight (12323, plus `poll_transfer` bits from B5/B6), NVAL default (12968) | | leave as ALL. Count them with the existing `CHURN_STATS` alltag census (syscall.rs:13123) before touching them | |
| C5 | DRM syncobj herd knob | drivers/src/drm_device_interface.rs:3294 (only with `drm.syncobjherd=1`) | leave (debug A/B knob) | |

Checked and narrow already: wait4/waitid (WAIT tag), nanosleep/sigsuspend/sigtimedwait (TIMED_ONLY or signal tag),
futex (own channel), pipes, ptys, eventfd and timerfd (keyed/tagged in `fd_wake_tag` and poll), splice/tee (pipe
tags; the non-pipe end goes through the A6/B hints), pidfd, AF_UNIX connected/pending/listener, DRM and evdev polls,
netctl poll, f2fs/iostat/hungtask/usb_storage sleeps (TIMED_ONLY), and the net daemon (NETD_TAG).

Not a park, but seen on the way: `push_spool_blocking` (servers/pipewire/src/lib.rs:146) busy-spins with the STATE
lock held while the device queue is full. That is worth a look for the "x86 audio clipping" item.

## Lanes (ranked by expected wake volume on the laptop)

Reserve the class numbers up front so the lanes do not collide in sched/src/lib.rs `poll_class`: `LISTEN = 20`,
`SOCKSTATE = 21`, `FLOCK = 22`, `STATIC = 23`. The only shared file is that one-line-per-lane addition. Lanes 2 and 3
build on lane/pollnarrow, so merge pollnarrow first.

### 1. `listentag`: TCP listener (A1). **Measured ~2,777 wakes/s** (sshd tokio worker, plus sshd's 8 % sys)
- Files: servers/net/src/listen.rs (`maintain`/`maintain_one`/`destroy` return or push the lid key),
  servers/net/src/lib.rs (the `InetListening` arm of `handle_poll`, the two `listen::maintain` call sites in the
  daemon, and the listener close path), sched/src/lib.rs (the `LISTEN` constant).
- Missed-wake risk: **high impact**. A missed edge means sshd stops accepting, and nothing else rescues it on an idle
  link, because the 100 ms broadcast fires only while smoltcp is active. The key point is that edges must be produced
  wherever `l.seq` moves (listen.rs:212), which is the same condition that drives EPOLLET today, so the seq and the
  wake can never diverge.
- Test: in regress, a tokio-style EPOLLET acceptor on a loopback listener and on a NIC (hostfwd) listener. Connect 50
  times with 100 ms idle gaps and require each accept within 20 ms. A negative control with the wake removed must
  FAIL. Then check the `/proc/sysstat` `w` line for the acceptor: mask not all ones, wakes ≈ connects. On the laptop,
  rerun `zc/m.sh`: the sshd row should drop below 100 wakes/s and sshd below 1 % CPU.

### 2. `vfstag`: VFS blocking-read tags + flock (B1, B2, B3, B4, B6, B7). Expected tens to ~100 wakes/s, unmeasured
- Files: servers/vfs/src/lib.rs (`fd_wake_tag`, the always-ready `poll_vnode_k` arm, F_SETLKW/flock and lock
  release), servers/pipewire/src/lib.rs (VFS_POLL arm).
- Missed-wake risk: medium for B7 (a release path that does not wake; keep a 1 s backstop) and for B1 (only if a
  server's tag differs from what its producer wakes; use the server's own VFS_POLL answer, never a guess). B2, B3, B4
  and B6 are low.
- Test: (a) a blocking evdev read with background AF_UNIX churn. The reader's `w` row should show a narrow mask, and
  `sendkey` should return it within 20 ms. (b) A blocking DRM read gets the flip event. (c) A blocking signalfd read
  returns on `kill -USR1` from another process. (d) Two processes on one flock, released by LOCK_UN, by close, and by
  exit: the waiter must acquire within 20 ms each time. (e) An EPOLLET interest on /dev/null plus a pipe: the waiter
  wakes only on pipe writes.

### 3. `nettag2`: rest of the net server (A2-A6). Expected <50 wakes/s (blocking DNS recv in relibc, ping, sockets mid-setup)
- Files: servers/net/src/lib.rs (the other `handle_poll` arms, `fd_wake_tag`, and bind/connect/listen/first sendto
  waking `SOCKSTATE`). Same file as lane 1 but disjoint hunks; run it after lane 1 or rebase.
- Missed-wake risk: medium for A6 blocking `connect` (handshake completion must move `readiness_moved`, which it
  does through the socket's sig) and for the A3/A4 state transitions when another thread is parked. Low for A2.
- Test: blocking `recv` on an idle TCP connection returns within 20 ms of the peer's send on a quiet VM (no other
  traffic, so no broadcast safety net). `ping -c 3` works and its `w` row is narrow. A blocking `connect` to a
  hostfwd port succeeds and one to a closed port is refused promptly. Thread A polls an unbound UDP socket with
  EPOLLET while thread B binds it and a datagram arrives: A wakes.

### 4. `kparks`: kernel console poll, empty sets, VT wait (C1, C2, C3). Expected ~0 on the laptop desktop, but each hit wakes on every wake in the system
- Files: kernel/src/syscall.rs (13028 console arm, `poll_block`/`poll_fds_mask`/`select_fds_mask` empty-set
  handling), servers/tty/src/vt.rs:1110. It overlaps pollnarrow's syscall.rs region only by file (6399 vs 12700-13030),
  and uses pollnarrow's console tag, so it starts after pollnarrow merges.
- Missed-wake risk: C1 medium (keypress latency at a console prompt if the console tag misses a source; the serial
  drain must wake it, which is the same contract as the 6399 fix). C2 and C3 low (deadline/signal only; the VT switch
  already broadcasts).
- Test: a program that `poll()`s fd 0 on the console under background churn has wakes ≈ keypresses, and a
  `sendkey`/serial byte returns it within 20 ms. `poll(NULL, 0, 500)` sleeps ≥ 500 ms with ~0 wakes in its `w` row.
  `VT_WAITACTIVE` returns within 30 ms of `chvt` from another process.

Ranking caveat: only lane 1 is measured. After pollnarrow and lane 1 land, rerun `zc/m.sh` on the laptop, list every
`w` row whose mask is still all ones, and read the `[CHURN] alltag` census (syscall.rs:1263, index 0 inet/listener,
2 signalfd, 3 inotify, 4 device, 5 stdio, 6 file). Use that to confirm or reorder lanes 2-4 before starting them.
