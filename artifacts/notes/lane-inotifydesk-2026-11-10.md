# lane/inotifydesk (L5) 2026-11-10 - FLIPPED: vfs.inotify is on by default (vfs.inotify=0 opts out)

Evidence (aarch64 HVF, COSMIC --virgl session, leandro logged in, 60 s settle, 123 s idle window, inotifydesk.py):
- total busy ticks over the window (4 CPUs, 49000 ticks): off 149 (0.30 %), on 224 (0.46 %). Difference 0.15 points.
- inotify counters (new /proc/sys/fs/inotify/{events_queued,events_coalesced,events_overflow,instances,watches}): on = 35 instances, 54 watches, 1 event queued since boot, 0 during idle, 0 coalesced, 0 overflow. No config feedback loop, no storm.
- per-process CPU and context-switch columns of the sampler came out empty on this guest (stat/status parsing under the guest shell), so the wake rate is the event count (0/s), not a context-switch rate.
- Firefox: ffsandbox.py aarch64 --mode on: RESULT on True, all children Seccomp 2, 0 sandbox log lines.
- Bench (inotifytest bench <dir>, 4 KiB pwrite x20000 and create+unlink x3000, best of 5): 200 unrelated watches vs 0, knob on vs off: within 3 % everywhere, /run and /root.
- podpasta, PODPASTA_CHURN=10: churn fails=0, no pasta left, no netns left. pasta_leaks "mounts: 1" is the baseline /run/netns mount; pasta_portfwd fails because the image has no httpd (both pre-existing, see lane-pastaleak).
- inotifytest default: 27 passed 0 failed; with vfs.inotify=0: t_killswitch PASS.

Changes: inotify::enabled treats only `vfs.inotify=0` as off; inotifytest knob_on/killswitch logic inverted; counters; `inotifytest bench`; inotifydesk.py sampler.
