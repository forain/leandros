# lane/mediabus — 2026-10-17

## Question

Laptop, Firefox playing YouTube: cosmic-panel 46 %, busd 35 %, cosmic-session
25 %, cosmic-settings(-daemon) 16 %, pipewire 14 %, pipewire-pulse 8 % of a CPU
(idle session: 1–2 % each). Suspected a D-Bus signal storm (MPRIS etc.) that
busd fans out to everyone.

## Answer: not D-Bus. Kernel wake herd from a saturated 64-bit wake bloom.

Evidence:

- **busd routes zero messages during playback.** New opt-in census
  (`ports/busd/wire-stats.patch`, `touch $XDG_RUNTIME_DIR/busd-stats.on`,
  read `$XDG_RUNTIME_DIR/busd-stats`): `0 messages, 0 deliveries` per 5 s
  window, on the old and the new kernel, Firefox (as leandro, on the session
  bus) playing a VP9 video in a COSMIC session. busd's match-rule handling is
  dbus-daemon-like already (signals go only to peers with a matching rule or
  the unicast destination). In the laptop run Firefox was not even on the bus
  (root, no DBUS_SESSION_BUS_ADDRESS, "Cannot spawn a message bus").
- **/proc/wakestat**: each AF_UNIX send (`net:3760`) woke ~10 parked tasks,
  each recv that freed space (`net:4764`) ~18, each eventfd write ~12 — where
  0–1 is the real interest. Laptop idle: 7–10 woken per AF_UNIX call.
- Mechanism: parked pollers record a 64-bit bloom (`poll_mask`), one bit per
  interest tag. A tokio reactor watching AF_UNIX sockets with
  EPOLLIN|EPOLLOUT registers three tags per socket (state, RD, WR); busd with
  ~45 clients fills ~60 of 64 bits, cosmic-session (child pipes + sockets) and
  the panel (a Wayland server for the applets) similar. So nearly every
  AF_UNIX/pipe/eventfd wake in the system — and Firefox's processes plus the
  compositor generate thousands per second during video — made them Ready to
  re-probe 40+ interests (vfs/net round trips under global locks) and park
  again. That kernel time is charged to their utime.
- signalfd answered `POLL_TAG_ALL`, so every SPA loop (pipewire,
  pipewire-pulse, leandros-snd-sink) parked with the broadcast mask and was
  woken by *every* wake (census: mask_bits 64).

## Fix (kernel, 0305c1dc)

- `sched::poll_key(class, index)` (the full splitmix64 whose low 6 bits are
  the old tag bit). Connected AF_UNIX data edges, pipes and eventfds wake with
  `wake_poll_key` / `try_wake_poll_key`; their NET_POLL / VFS_POLL replies
  carry the same keys (data[33] marker `0x4b`, count data[34], keys
  data[40..64]); one helper per object kind (`unix_rd_key`, `unix_wr_key`,
  `pipe_key`, `eventfd_key`) feeds both sides.
- epoll/poll/select collect a `PollSet`: bloom mask + which bits ONLY keyed
  interests explain (`poll_kbits`) + 16-bit fingerprints (≤ 512, 1 KiB per
  task). A keyed wake whose bit is in `kbits` reaches the task only on a
  fingerprint match. Safety: unkeyed contributions, fingerprint overflow,
  `widen_poll_masks`, and legacy `block_on_poll_prepare_masked` parks are never
  refined; bit-only wakes ignore refinement. Nested epoll fds are expanded into
  their inner interests' tags so keys survive (`epoll_collect`).
- signalfd: narrow tag `poll_tag(SIGNALFD, tgid)`; every signal post already
  issues a broadcast poll wake, which reaches any non-zero mask.
- Diagnostics: `/proc/wakestat` `# keyed skipped N fp_hit N unrefined N`;
  with `sched.wakestat=1` also `park pid tgid mask_bits kbits keyed unkeyed`
  (each thread's last poll-set park) and a 1024-entry outcome table (was 128,
  overflowed in a desktop session).
- Test: `epolltest` `unrelated_unix_traffic_no_wake` — a thread parked on 48
  socketpair ends + a quit eventfd; 300 spaced pings round-robin over 32
  unwatched pairs; narrow-mask tasks woken at the AF_UNIX sites (woken −
  allmask) with the waiter minus a no-waiter control pass must be < 1/8 of the
  wake calls. Deterministic: e93d2049 281/600 (FAIL), mediabus 17/600 (PASS),
  three runs each.

First cut had `POLL_FP_MAX = 128`: busd (135 keys) overflowed and stayed
unrefined — the park census showed it (`keyed 44 unkeyed 38`).

## Measurements (QEMU aarch64 HVF, virgl, COSMIC session as leandro, Firefox
playing a looping 480x270 VP9 file, 12 s utime window, same root image)

| process | e93d2049 | mediabus |
|---|---:|---:|
| total (all processes) | 152.1 % | 93.2 % |
| firefox | 32.3 % | 24.1 % |
| cosmic-comp | 28.1 % | 26.2 % |
| cosmic-panel | 15.9 % | 9.9 % |
| busd | 11.8 % | 5.3 % |
| cosmic-session | 8.4 % | 3.5 % |
| cosmic-panel-button (sum) | 5.8 % | 2.8 % |
| cosmic-settings-daemon | 4.8 % | 1.8 % |
| pipewire | 3.3 % | 1.2 % |
| pipewire-pulse | 2.2 % | 0.9 % |

Tasks woken per second, top sites: AF_UNIX recv 11 355 → 4 531, send
9 496 → 4 272, eventfd 4 076 → 1 444, eventfd (IRQ path) 3 854 → 1 442, pipe
2 051 → 526. busd messages routed: 0 → 0.

## Tests (release)

- regress aarch64 24/24 PASS, x86_64 24/24 PASS (0305c1dc kernel).
- final f5fe13dc: aarch64 24/24 PASS; x86_64 23/24 — sshsmoke failed once
  (5 ssh checks, 96 s), then `--suite ssh --repeat 2` 2/2 PASS on the same
  image: flaky host-side ssh, not this change (it passed on 0305c1dc, which
  already had every kernel change).

## Laptop

Kernel built, NOT deployed: `LEANDROS_KERNEL_TAG=mediabus-f5fe13dc`
(`target/final-x86_64/kernel`, sha256 1ab76f83…0464c). This is a kernel fix; no userland change is
needed for it. Optional userland: `/usr/libexec/busd` (adds the opt-in census
only). Expect busd/session/settings/pipewire to drop by more than half during
playback; `/proc/wakestat` `# keyed` shows skipped ≫ unrefined.

## Left open

- cosmic-panel ~10 %, cosmic-comp: remaining wakes are mostly real or from
  unkeyed tags (timerfd expiries wake by bloom: `sched/lib.rs` deadline site
  ~3 000 woken/s from ~270 expiries/s — key timerfds next; DRM event tags).
- Broadcast-mask parkers remain: sshd (inet listener reports no tag), fd 0-2.
- Signal posts are broadcast herds (`sched/lib.rs:1412`, ~190 woken per call).
- Building needed a pinned brush copy: the shared `../brush` checkouts are
  modified by another lane (`LEANDROS_BRUSH_DIR`).
