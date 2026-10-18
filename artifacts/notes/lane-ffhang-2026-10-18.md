# lane/ffhang — 2026-10-18

Firefox "hung" on the laptop under main b6e6f363 (procps2 + epollready2) but
played YouTube under main 63bcebe2. **There is no kernel regression.** The
A/B was confounded: the screen was blanked (cosmic idle) in every failing
run and lit in every passing one. With the screen off, Firefox does the same
thing under 63bcebe2.

## Result

| kernel | screen | Firefox + YouTube |
|---|---|---|
| 74d2170c (procps2 only), fresh login | on | plays: firefox 87 %, Web Content 53-58 %, RDD 17-20 %, pipewire-pulse 6.7 %, snd-sink 5-6.7 %, wifi RX 3-9 MB/min |
| b6e6f363 (+ a read-only /proc/sysstat probe), **ready list ON**, 7 restarts (3 of them with `ps -e -o args` and `ps -T` run in a loop during startup) | on | 7/7 play: firefox 100-117 %, Web Content 56-113 %, RDD 7-70 %, pipewire-pulse 3-7 % |
| b6e6f363 (same build), Firefox started after the session had been idle about 15 min | **off** | stuck: all processes ~0 % CPU, wifi RX +8 KiB in 40 s |
| **63bcebe2 (good)**, Firefox started after the session had been idle about 17 min | **off** | **same stall**: ~0 % CPU, +8-9 KiB in 40-60 s, still stalled 3 min later |

## Evidence that the original failure was a blanked screen

Laptop klog `artifacts/remote-logs/20261018-041004-172.16.149.179/klog-all.txt`
(main checkout), boot 26 = b6e6f363 with the ready list on:
- The i915 `flips N/s` stats stop about 20 min after boot (the greeter's idle
  screen-off: a fade burst of ~14 flips/s, then nothing). Firefox was started
  about 2 h later (line ~4287). The only later i915 line says `flips 0.0/s`.
  For comparison, boot 25 (63bcebe2, the "good" run) started Firefox about
  10 min after boot, and its playback showed 45-53 flips/s.
- The network stayed silent. Over the whole boot there were 15k RX frames,
  12.7k of them at 1 Mb/s (broadcast). There were 3.7k TX frames, and all but
  3 were ACKed. The TID 4 BA session that the AP opened for the video reached
  SSN 15. So the radio worked, but Firefox never issued its requests.
- On this Mac's runs, the screen-off state is the only thing that turns a
  passing start into the stalled one, on both kernels.

While stalled, every Firefox thread is in a normal idle wait. The main
thread is in poll() (sc 7, the glib loop on the Wayland fd); the IPC I/O
threads are in epoll_pwait (281, listener on); the rest are in futex (202).
That is the shape of a client waiting for the compositor, which sends no
frame callbacks while the output is off. It is not a lost wake.

## The `epoll.ready=0` boot (Web Content at 3.4 cores, no playback, up 4.4 h)

Four Web Content threads were spinning, almost entirely in user mode: IPC
I/O Child, TaskController #5, MediaSupervisor #41 and MediaDecoderStateMachine
#3. Together they made about 5k futex calls/s, half returning 0, and no
epoll storm. wifi0 RX was +2 KiB/10 s. This was not reproduced. It fits a
media pipeline left without a presenting compositor after the screen
blanked (the clip ends after 10 min, which releases the idle inhibit), but
that is unverified. The screen cannot be woken remotely: there is no input
injection, and evdev write only records LEDs.

## Side findings (not bugs behind this report)

- Per-thread `/proc/<pid>/task/<tid>/stat` field 4 (ppid) shows the creating
  THREAD (`6845 ... S 6843`). Linux shows the process's parent for every
  thread. This is cosmetic; procps2 reuses `proc_stat_of(tid)`.
- The Mac disk ran full twice during the bisect: ~13 agent worktrees of
  ~17 GB each sit in `.claude/worktrees/`. A full `build-all.sh` in a fresh
  worktree needs about 6 GB. For a laptop kernel A/B, build just the kernel
  (the cargo line of `build_kernel()`, ~40 s incremental) and reuse the
  init-only initrd from the ESP: `remote.py espctl get
  /leandros/k/<good>/initrd.gz /tmp/x` + `remote.py pull`.
- Diagnostic used here, not committed: `/proc/sysstat` `w` lines (`w pid
  tgid state sc port listen poll_mask` per live task, from `sc_saved_nr`,
  `blocked_on`, `listen.on`). These show where every thread of a hung process
  waits. Patch: scratchpad `ffhang-sysstat-w-lines.diff`.

## Test recipe (laptop)

1. Make sure the panel is lit, e.g. a fresh login within ~15 min, or someone
   touches the laptop. Compare kernels only under the same screen state.
2. Restart Firefox with `fftrial.py` (scratchpad tools/): it kills the
   Firefox family by `ps -C` name, starts it, and samples 30 s of per-process
   CPU plus wifi0 RX. Its `ps` argument hammers procfs during startup.
3. "Playing" means RDD > 5 %, pipewire-pulse and snd-sink 3-7 %, and wifi RX
   of MB per minute.

## Laptop state at hand-off

Booted good 88e5c26c4479 (63bcebe2), default=good, not armed. The trial slot
now holds db233fe17505 = b6e6f363 + the sysstat probe (the old trial
5b41169bd9a7 was replaced by the bisect deploys). Nothing was promoted.
