# lane/epollready — 2026-10-17 (HANDOFF)

Branch `lane/epollready` off local main d2f0eaee (Merge 'lane/wifiht').
Worktree: /Users/forain/code/leandros/.claude/worktrees/agent-a09ce9f98fd2fcb5a

## Problem

On the laptop during YouTube, desktop threads sit in epoll_pwait; every wake
re-probed EVERY registration (VFS_POLL / NET_POLL round trip each), twice per
park (the probe pass, then `epoll_any_ready` between prepare and commit), and
every call started with a full pass. ~300 µs kernel CPU per useless wake;
cosmic-comp 613 µs per epoll_pwait over ~41 fds (lane deskbusy2).

## Design (commit 8fe32140)

Like Linux's ready list, a wake re-probes only what it can concern.

- **Poll-wake listener** (`sched::PollListen` in `Task::listen`, RunQueue
  `listening` bitmap + `ls_mask` mirror). From a thread's first epoll_wait on
  an instance until it parks on something else, the wake scans
  (`unblock_port_tagged_inner`, `wake_due_poll_deadlines_keyed` for timerfd
  expiries, `widen_port_masks`) record every wake that passes its filter — the
  same mask / kbits / fingerprint rule as a parked poller — into a per-task
  `WakeRing` (8 exact keys; overflow and bit-only wakes OR into `tags`;
  `WAKE_DEADLINE` flag), whether the thread is parked, running, in user
  space, or blocked elsewhere. API in sched/src/lib.rs: `poll_listen_begin`
  (None = record-everything filter), `poll_listen_update` (install set, take
  ring), `poll_listen_take`, `poll_listen_park` (parks ONLY if the ring is
  empty, same RUN_QUEUE hold: Taken/Parked/Off), `poll_listen_end`,
  `poll_listen_enter`/`poll_listen_store` (take/put `Task::ep_cache` + ring).
  `block_on_port_until_keyed` (poll/select/legacy parks) ENDS the listener,
  because it overwrites `poll_fps`, which the listener filter shares.
- **Per-thread snapshot** `EpCache` (kernel/src/syscall.rs, stored as
  `Box<dyn Any>` in `Task::ep_cache`): per registration index its fd/ofd/mseq,
  armed, last probe tag (`PollTag`), `ready` (ready at last probe, or never
  probed). Valid while same slot, same instance `id`, same listener id.
- **`epoll_wait_ready`** (syscall.rs, selected in `epoll_wait_until` unless
  `epoll.ready=0` on the cmdline → `EPOLL_READY`): per pass
  DIFF instance vs snapshot (new/changed fd/ofd → unknown tag; `mseq` changed
  = ADD/MOD → candidate, old tag kept provisionally) → CANDIDATES = all if
  reason unknown; else unknown-tag, ALL-tag and nested entries (always),
  ready-list entries (first pass of a cached call), and `ep_hit` (key equal to
  one of the entry's keys, or the key's bit among the entry's UNKEYED bits;
  bit-only tags intersecting the entry's bits) → PROBE candidates with the
  old loop's exact fire/ET/ONESHOT/POLLNVAL rules → FILTER: if any tag
  changed (or the listener is still ALL), rebuild the PollSet and
  `poll_listen_update`; entries whose tag changed under a narrow filter are
  re-probed once more (`recheck`; after 2 rechecks probe under an ALL filter
  — `churn` guard) → return / park via `poll_listen_park`, EPOLL_CTL_GEN
  re-checked after Parked (as before). After a real sleep: ring empty ⇒
  reason unknown ⇒ full re-probe (counted in `EP_UNKNOWN`).
- Cache miss (first call, other instance, poll()/select() in between):
  `poll_listen_begin(None)` (record everything) BEFORE the full pass, so no
  second full pass is needed; then the real filter is installed.
- `EpollInterest::mseq`, `EpollInstance::id` from `EPOLL_SEQ`.
- `/proc/wakestat`: `# epoll calls N miss N probes N unknown N parks N`.

### Invariants (a lost wake is a hang)

1. While a listener is on, every poll wake matching its filter is in its ring
   (or its bits are). Readers take the ring under RUN_QUEUE; park only if
   empty in the same hold.
2. An entry is probed under a filter that covered it, or is re-probed after
   the new filter is installed (recheck), or was probed under ALL.
3. Unmatched keys are ignored — sound under the existing mediabus/deskbusy2
   rule that a keyed wake's key equals the key the object's probe reports, or
   the probe reports that bit without a key (then `ep_hit` catches it), or the
   object is foreign (never keyed-woken). Bit-only wakes select by bits (the
   tag invariant: producer bit ∈ probe bits for the same object).
4. Events written + committed (last_seq/ONESHOT) in a pass are always
   returned (the listener-lost path no longer `continue`s when n > 0).
5. Readiness that changes with NO wake at all used to be rescued by unrelated
   full re-probes; now only by the ready list / next unknown wake. Any such
   latent producer bug surfaces as a stall. Watch for it.

## Verified

- Builds release both arches (`./scripts/build-all.sh`, EXIT 0).
- aarch64 HVF: `epolltest` 23/23 PASS (15 old + 8 new) on the new path, AND
  23/23 with `epoll.ready=0` (legacy loop) — tests-epr-a1, tests-epr-a1-legacy.
- New cases (commit 08c3cd0f): lt_stays_reported, et_once_across_calls,
  userspace_window (readiness + ADD while in user space), keyed_and_unkeyed_burst
  (eventfd+pipe+timerfd+signalfd with 30 idle fds), wake_ring_overflow (40
  eventfds, in user space and parked), nested_epoll_warm, ctl_del_while_parked,
  oneshot_rearm_warm.

## Measurements (aarch64 HVF, `epolltest bench`, kernel ns from /proc/sysstat)

| bench | legacy (epoll.ready=0) | ready list |
|---|---:|---:|
| roundtrip: 41 fds, one ready per call (cosmic-comp pattern), kernel ns/call | 44 566 | **7 437** |
| roundtrip wall µs/round | 55 | 27 |
| spurious: broadcast-mask waiter (/dev/tty in set, like sshd), 1000 unrelated AF_UNIX pushes, kernel ns per push | 68 791 | **6 715** |

wakestat over that boot: 2301 epoll calls, 26 misses, 12 930 probes (~5.6
per call incl. the test suite).

## NOT done (next steps, in order)

1. `--suite regress` on aarch64 and x86_64 (never ran: QEMU was busy with
   other lanes; the runner was cancelled at handoff).
2. `--suite net-e2e` on both arches.
3. epolltest + bench on x86_64 (A/B image ready: see below).
4. COSMIC session smoke + desktop measurement: prepared
   `ffsession.py aarch64 <tag> --put <S>/v.webm:/tmp/v.webm,<S>/v.html:/tmp/v.html
   --url file:///tmp/v.html --prefs "media.autoplay.default=0"
   --post "cat /proc/sysstat; sleep 12; cat /proc/sysstat; cat /proc/wakestat; grep -H . /proc/[0-9]*/comm"`
   then diff the two sysstat reads per tgid for epoll_pwait (aarch64 nr 22,
   x86_64 232/281/441): kernel ns per call for cosmic-comp / busd / panel;
   same with LEANDROS_DISK_IMAGE=<legacy image> for before. (Check
   ffsession's `--put` syntax first.) Desktop must render + respond.
5. Optional: poll()/select() (relibc's poll() is an epoll instance per call →
   always a cache miss; still gets the in-call savings).
6. Laptop: not touched (per instructions).

Scratch (A/B images, video, runner scripts):
/private/tmp/claude-501/-Users-forain-code-leandros/d7f39651-5cb0-4edb-ab21-fbb65b064269/scratchpad/epr/
- `legacy-<arch>.img`: boot image copies with cmdline `epoll.ready=0`
  (`LEANDROS_DISK_IMAGE=...`). Rebuild them after a rebuild
  (`scripts/set-image-cmdline.sh <copy> "epoll.ready=0"`).
- `run.sh <arch> <tag> <runid> [runtests args]`: waits for no QEMU, runs.
- `v.webm`/`v.html`: 480x270 VP9 loop page.

Test commands:
`LEANDROS_RUN_ID=<id> python3 .claude/skills/run-leandros/runtests.py <arch> <tag> /bin/epolltest "/bin/epolltest bench" "cat /proc/wakestat"`
`... runtests.py <arch> <tag> --suite regress` / `--suite net-e2e`

## Pitfalls

- The session scratchpad is SHARED with sibling lanes: use a private subdir
  (another lane's build overwrote `scratchpad/build1.log`).
- `pgrep -f qemu-system` matches other agents' shell wait loops; check the
  executable name (`ps -A -o comm= | grep qemu-system`).
- Merge risk: lane sigfast edits signal paths in sched in parallel. This lane
  touches sched/src/runqueue.rs (wake scans, keyed park, deadline scan,
  widen), sched/src/lib.rs (new listener API after `block_on_poll_commit`),
  sched/src/task.rs (two fields at the END of Task + both constructors + the
  in-place constructor), wakestat.rs, servers/vfs (one wakestat line),
  kernel/src/syscall.rs (epoll), kernel/src/main.rs (knob). Signal paths that
  make a poll-parked task Ready directly (`clear_block_fields`) do not record
  in the ring — that is fine (empty ring ⇒ full re-probe), keep it that way.
