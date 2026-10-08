# Lane greeterdisp — 2026-09-27

Branch `lane/greeterdisp` from integ-wave-0924 `768c169c`, head `e6993c90`, pushed. Worktrees on both machines were removed; the branch is kept.
Laptop x86_64/KVM (UHD 620): Venus→ANV and virgl→iris. Mac aarch64/HVF virgl with the GPU QEMU
(`~/.local/qemu-gpu`) and `LEANDROS_GPU_STAGE` pointing at a symlink to `gpu-stage-aarch64.prev`. The shared stages were not touched.

## Result: FIXED (kernel). The premise was half right.
The "~1000 dispatches/s" is the whole guest, not the greeter alone. After compchurn the baseline was about 550–580/s. The idle greeter
is **not** in an app-level redraw loop. It redraws at **2 Hz**, driven by the password caret's blink (a timerfd at 500 ms).
Each blink is one wl_surface commit, one compositor flip (2 DRM flips/s) and a few Wayland round trips. There is no 1 ms timer,
no frame-callback storm and no animation that never settles. Per second, the greeter returns from epoll with events about 20 times:
- about 12 on the Wayland socket;
- about 2 each on three eventfds (tokio/calloop wakers);
- about 2 on its timerfd.

Its futex handoffs are about 13/s. On Linux that is the whole wakeup budget.

On our kernel each useful wake cost about 5 more, all **spurious**: woken, re-probed, nothing ready, parked again. Five kernel causes,
all fixed. None of the four suspects in the brief was it:
- there is no epoll level-trigger misreport;
- no eventfd or timerfd stays readable;
- EAGAIN after POLLIN on the Wayland fd is 0 in excess (`eagain` == events);
- flips are paced by real fences (2/s).

| # | cause | commit |
|---|---|---|
| 1 | `epoll_ctl` woke every waiter of the instance on **every** ADD/MOD/DEL. calloop/`polling` re-arm EPOLLONESHOT with MOD after each event, which made about 75 ctl/s. Linux wakes only if the re-armed item is ready. | `ac5ffcef` |
| 2 | A DRM card fd's VFS_POLL reported `POLL_TAG_ALL`, so cosmic-comp's main epoll mask was a **broadcast mask**: woken by every wake in the system (`park_all` = 30/s). Each delivered flip also woke **every** parked poller (about 24 threads per flip). | `6610b5dd` |
| 3 | AF_UNIX data wakes used one tag per connection for both directions. A client parked for POLLIN on its Wayland fd was woken each time the compositor *drained* what it had sent, and each time a sibling thread sent. | `d5038efe`, `e6993c90` |
| 4 | nanosleep, sigsuspend and sigtimedwait parked with `POLL_TAG_ALL`, so every targeted wake in the system woke every sleeping thread. That was about 55/s for one `sleep 60`, and the same for init's getty loop. | `0e6bafb` |
| 5 | The smoltcp daemon's 100 Hz park, same issue: +25 passes/s. | `e53c92cc` |

### Fixes
1. **epoll_ctl** (`kernel/src/syscall.rs`, `sched`):
   - on ADD/MOD, probe the fd. If it is ready now, wake the instance's tag, as Linux does. If not, `sched::widen_poll_masks(self_tag, fd_tag)` ORs the fd's tag into every parked mask that carries the instance tag, without waking it.
   - DEL wakes nobody.
   - A global `EPOLL_CTL_GEN` is read before each probe pass and re-checked after publishing Blocked in epoll_wait, poll and select. That closes the probe-to-park window. A ctl after the re-check finds the waiter already Blocked, and the widen reaches it.
2. **DRM**: `drm_event_poll_tag(open)` = `poll_tag(DRM, 0x4000_0000|open)`. The VFS_POLL reply carries it (`data[32]=1`). Both READY_EVENTS producers wake only the opens they queued for. A contended IRQ-context wake is now deferred with `request_poll_wake_tagged` instead of dropped.
3. **AF_UNIX**: new classes `UNIX_RD`/`UNIX_WR`, indexed `conn*2+end`.
   - A send wakes the reader end's RD tag, and a drain wakes the writer end's WR tag.
   - NET_POLL takes the requested events, so an end's tag is the connection tag plus RD if POLLIN is requested and WR if POLLOUT is.
   - Close, shutdown, death, accept and pre-accept traffic still wake the connection tag, which every mask keeps.
   - A pre-accept connector already reports end A's full tag. That covers the window of probe → accept+wake → park.
   - `fd_wake_tag` (blocking send/recv) returns the full end tag.
4. **`POLL_MASK_TIMED_ONLY` (0)**: `unblock_port_tagged` now lets a `POLL_TAG_ALL` wake reach *any* mask, including 0. Every signal delivery wakes with ALL. Sleeps and signal waits park with mask 0, so only their deadline or a signal wakes them.
5. The net daemon uses the same mask. Inet producers already broadcast.

## Numbers (idle greeter, 60 s window, 10 s census windows averaged; x86 = laptop KVM, a64 = Mac HVF)
| path | build | guest disp/s | greeter disp/s | comp disp/s | greeter CPU % | comp CPU % | host QEMU CPU % |
|---|---|---|---|---|---|---|---|
| x86 Venus | base | 584 | 125 | 159 | 0.65 | 2.68 | 7.4 |
| x86 Venus | fixed (4 boots) | 334–353 | 62–74 | 143–154 | 0.49–0.52 | 2.4–2.6 | 6.9–7.2 |
| x86 virgl | base | 546 | 125 | 58 | 0.62 | 3.10 | 7.7 |
| x86 virgl | fixed (4 boots) | 235–246 | 62–75 | 38–43 | 0.46–0.54 | 2.7–2.9 | 7.3–7.4 |
| a64 virgl | base (2 boots) | 558–574 | 137–141 | 50–54 | 0.72–0.75 | 4.4 | 8.5–8.7 |
| a64 virgl | fixed (2 boots) | 261–269 | 80–87 | 48–51 | 0.26–0.32 | 2.0–2.6 | 3.9–4.7 |

Greeter spurious wakes went from about 100/s to about 30/s. What remains is 64-bucket tag-hash collisions, mostly eventfd slots (Mesa makes a new fence
eventfd per frame) and timerfd bits hitting unrelated masks.

The idle greeter's CPU drops: x86 0.65 → 0.5 %, a64 0.74 → 0.3 %. The a64 host QEMU CPU halves: 8.6 → 4.3 %. The x86/KVM host cost barely
moves, 7.4 → 7.0 %; it is not dominated by guest dispatches.

### What remains, and why it is app/driver behaviour (Linux would do the same)
- cosmic-comp on **Venus**: two threads (pids 64/56) hand off through futex about 44 times a second each. Every wait returns 0, with no EAGAIN and no timeout, so these are genuine FUTEX_WAKEs. The same threads also make:
  - 160 clock_gettime/s and 74 gettid/s;
  - 23 sched_yield/s;
  - 33 fcntl/close/ioctl/s (fence fds).

  That is about 22 handoffs per 2 Hz frame, from Mesa Venus/the compositor's render path. It is not a kernel cause. virgl does not do this; its comp runs at 39–58 disp/s.
- cosmic-comp on **virgl**: 50 zero-timeout poll()s/s on fence eventfds (Mesa virgl fence polling). They do not park, so they cost no dispatch.
- The greeter: about 20 epoll returns/s plus about 13 futex/s at 2 Hz caret blink. A Linux client with a blinking caret wakes on the same sources at the same rates: the timerfd, and the frame callback, buffer release and ack events on the Wayland fd.

## Found, not fixed
- `init` pid 1, "sc=-", about 100 dispatches/s: that is the **net daemon kernel task**, a 100 Hz smoltcp poll. virtio-net RX has no IRQ path, so it
  must poll. Making it adaptive (smoltcp `poll_delay`, and none at all with no inet sockets) would take the idle guest from about 340 to about 240/s.
- **Console read(0) parks with a 10 ms ALL-mask poll** (`sys_read_impl`, `block_on_poll_prepare_until(now+10ms)`). A serial shell sitting at its prompt costs
  about 100–130 wakes/s. It showed up in 2 of 7 x86 runs, where the driver's `sleep` returned early and brush went back to its prompt.
- Tag-hash collisions: 64 buckets. Widening `poll_mask` to 128 bits would roughly halve the remaining spurious wakes. That touches every tag producer.
- 1 of 6 x86 boots of the fix2 build came up with the password field **unfocused**: no caret, so no redraws, and 110 guest disp/s.
  - It was not seen in the fix3 boots or in 5 boots of earlier builds (3 baseline, 2 fix1), and typing works in every checked boot.
  - A lost-wake window I then found and closed (`e6993c90`, the pre-accept connector) is a plausible cause, but it is unproven.

## Tests (release builds, census ON unless noted)
- aarch64 HVF virgl: drmsmoke, epolltest 11/11, polltest, idletest, waittest, sigtest, sigtest2, jobtest, pthreadtest,
  wakepolltest 45/45, timertest, sigchldtest, scmtest, smpwaketest, racetest, ptytest, forktest and exectest all rc=0, with no FAIL lines.
- x86_64 KVM, **census-OFF** build (the committed configuration), both Venus and virgl: the same 18 tests all rc=0, with no FAIL lines.
- All boots of fix3 were focused and typing worked: x86 Venus 4/4, virgl 3/3, a64 2/2. None hit the virgl wedge that wgpusettings saw.

## Tooling
- Census, compiled-in but off: `sched::gdwake::ENABLED` + `sched::idlestat::ENABLED`. It prints:
  - `[GDT]`: per thread, epoll/poll outcomes, spurious wakes, EAGAIN, futex outcomes, park_all;
  - `[GDFD]`: per (tgid, fd, kind), events delivered;
  - `[GDSITE]`: per (waker file:line, tgid), useful vs spurious;
  - `[GDSC]`: per (pid, nr), syscalls.
- Laptop scripts: `~/Projects/gd-{build,idle,pair,rep,focusrun,tests}.sh`, `gd-sum.py` (summarizer) and `gd-focus.py` (focus probe, 1920x1080). Output is in `~/Projects/gd-out/`.
- Mac scripts and output: in the session scratchpad (not kept).

## Shared files touched
- `kernel/src/syscall.rs` (epoll_ctl/epoll_wait/poll/select, sleeps, census);
- `sched/src/lib.rs` and `sched/src/runqueue.rs` (wake matching, widen);
- `servers/net/src/lib.rs`;
- `servers/drm/src/lib.rs` and `drivers/src/drm_device_interface.rs`.

The polltimer, runqlock and compchurn lanes were in these areas.
