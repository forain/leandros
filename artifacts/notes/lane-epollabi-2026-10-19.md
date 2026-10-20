# lane epollabi — 2026-10-19

Bug list items L2.5 and L2.6 (openbugs-2026-10-19.md).

## L2.5 epoll_ctl ABI — FIXED

Checked against Linux `do_epoll_ctl` / `ep_loop_check` (EP_MAX_NESTS = 4):

- **ELOOP**: an ADD of an epoll fd now walks the instance graph under
  EPOLL_INSTANCES (`epoll_loop_check`): refused if it closes a cycle, or if the
  longest chain down from the added instance + 1 + the longest chain up from the
  instance added to exceeds 4 links (so 5 instances in a chain are fine, a 6th is
  not, whichever end it is added at). Before: cycles and any depth were accepted
  (the probe's depth cap only hid them).
- **EPOLLEXCLUSIVE EINVAL**: on MOD; on ADD of an epoll fd; on ADD with any bit
  outside IN|OUT|ERR|HUP|WAKEUP|ET; MOD of an exclusive registration. Before: the
  flag was stored and ignored.
- **EPOLLEXCLUSIVE wake-one**: `sched::PollSet::add_excl` records the exact keys
  of exclusive registrations (vetoed by any non-exclusive interest naming the same
  key, off for broadcast/nested sets); they ride the park (`Task::poll_xkeys`,
  also through the listener's `listen_park`). A keyed wake on such a key wakes the
  first exclusive parker and leaves the others parked (`wakestat::EXCL_SKIPPED`,
  `# pollx ... exclskip`). Bit-only wakes still wake all (Linux allows "one or
  more"); sockets that wake bit-only (TCP listeners) therefore still herd.
- Already right: EINVAL for epfd == fd (also via a dup), non-epoll epfd, bad op;
  EBADF; ENOENT.

Test `epolltest abi` (in `regress` and the new `epollabi` suite): 36 cases.
Before (main 07204f1b kernel): 14 FAIL (all ELOOP/EXCLUSIVE ones; wake_one woke 3
of 3). After: 36/36, `excl: exclusive=1 woken=1`, control `exclusive=0 woken=3`.

## L2.6 poll() cost — FIXED (premise partly refuted)

- relibc's poll(): it does build a fresh epoll per call, but only the
  RELIBC_LINKED test programs use it (lane nowake): the desktop (Firefox, COSMIC)
  is musl, whose poll() is the kernel's ppoll. Changed anyway: relibc poll/ppoll
  now call the kernel's ppoll directly (relibc branch `leandros/epollabi`
  9928b292 in /Users/forain/code/relibc-epollabi, on master 10ef96ee; not merged
  into relibc master — build with LEANDROS_RELIBC_DIR or merge it there).
- Kernel zero-timeout / single-fd path (what Firefox's VA-API pool hits): a probe
  of an ordinary descriptor looked it up three times (`resolve_fd`,
  `fd_is_console_stdio`, `handle_poll` behind `vfs::handle`). New
  `vfs::poll_fd_direct` answers it in one fd-table pass with the same
  revents/seq/tag (console, socket/epoll aliases, closed fds fall back). Used by
  poll, select and epoll probes.
- Woken poll re-probes: `poll_wait` now does one plain pass (returns at once when
  something is ready or timeout 0 — no tags, no listener, no RUN_QUEUE); to park it
  becomes a wake listener (`poll_wait_listen`): one tagged pass under the
  record-everything filter, then park; a wake re-probes only the entries its ring
  names. `poll.ready=0` on the command line restores the old loop.
- Also fixed: poll counted POLLNVAL entries as not ready (a closed fd blocked
  until timeout; Linux returns at once). Found by polltest `poll_linux_semantics`
  case 7 once relibc stopped emulating it.

`epolltest pollbench` (aarch64 HVF, release, under the qemu lock, 2 runs each;
getppid = 1.15–1.3 µs is the syscall floor):

| | before | after |
|---|---:|---:|
| relibc poll(1 eventfd, 0), idle / ready | 11.2 / 10.6 µs | 1.24 / 1.21 µs |
| relibc poll(8 eventfds, 0) | 38.2 µs | 1.70 µs |
| kernel ppoll(1, 0), idle / ready | 2.19 / 2.21 µs | 1.31 / 1.22 µs |
| kernel ppoll(8, 0) | 7.2 µs | 1.71 µs |
| blocking ppoll over 8, woken via one: probes per call | 32 (counted from the old loop: pass, tags, re-probe, pass) | 17 (pass, tagged pass, 1) |
| same, wall µs per round trip | 34–46 | 23 |

On the laptop rate in rddspin (18.4k calls/s at 4.0 µs) the kernel-side cost per
call should drop by about the same ~0.9 µs out of the ~1 µs above the syscall
floor; not measured on the laptop.

## Tests (aarch64)

`epolltest abi` 36/36, `polltest` 28/28, `epolltest` 27/27, `epolltest cost` 8/8,
`wakepolltest` 45/45; regress 38/38 (aarch64), 39/39 after rebase on main c71ed0c6. x86_64 not run (lane rule).

## Open

- A woken poll still pays one full tagged pass before its first park (17 vs the
  ideal 9 probes for 8 fds): a listener begun before the first pass would cost a
  RUN_QUEUE hold on every non-parking call, which is the common case.
- EPOLLEXCLUSIVE has no effect for objects whose producers wake bit-only.
- relibc branch needs merging into relibc master.
