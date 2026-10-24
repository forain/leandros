# Lane ffbloom, 2026-10-24 — REFUTED in QEMU (openbugs L3.4)

L3.4: on the laptop pop-launcher and Firefox's WebExtensions process shared a bloom bit, so a wake
for one woke the other. Checked after lane/panelwake keyed the DRM / WAIT / PIDFD / signal wakes.

## Method
QEMU aarch64/HVF `--virgl`, main 2950a0ea (+ the table fix below), `ffsession.py aarch64 b2 --wait 60
--put snap.sh:/tmp/snap.sh --post "sh /tmp/snap.sh"`: COSMIC session, Firefox on about:blank (Firefox
starts its WebExtensions process by default), 60 s idle observation, then two `/proc/epollstat` +
`/proc/wakestat` snapshots 10 s apart, plus `comm`+cmdline of every pid. `an.py post.txt <tgids>`
diffs them. Files: `lane-ffbloom-files/`.

## Result (10 s idle window, run b2)
| process | tgid | parks | useful | useless/s | attributed to |
|---|---|---|---|---|---|
| pop-launcher | 257 | 0 | 0 | 0.0 | — (fully asleep) |
| pop-launcher plugin cosmic-toplevel | 274 | 30 | 0 | 3.0 | eventfd 10: 4 of 30, rest unattributed |
| Firefox WebExtensions | 607 | 1 | 1 | 0.0 | — |
| Web Content / Privileged Content / Utility | 649 / 723 / 715 | 1 / 6 / 0 | all useful | 0.0 | — |
| Firefox parent | 530 | 43 | 0 | 4.3 | (ep45) unattributed |
Cumulative since boot: pop-launcher 329 useless (185 on its own pipe 13, startup), WebExtensions 36.
No pop-launcher <-> WebExtensions cross wake remains: each side is asleep or woken usefully.
/proc/wakestat 10 s: every poll wake site tagged/keyed, `allmask` 0 at every site.

## Fix (tooling fidelity) — 07a96d89
`/proc/epollstat` rows are direct-mapped by instance id; 256 rows were too few for ~150 live
instances: in run b1 pop-launcher and one Firefox content process had no row at all and one Firefox
row read negative across the two snapshots (evicted + re-claimed). Now 1024 rows (~140 KiB static).
Regress aarch64 ffbloomreg: all pass except sshsmoke ssh_hostkey (known flake, passed on rerun).

## Open (not L3.4)
- ~25 idle COSMIC processes each show 3-5 useless wakes/s with no fd attribution (total 105/s);
  same pattern in cosmic-toplevel and the Firefox parent. Not a shared-bit cross wake between two
  named processes; candidate: the poll-deadline site `sched/src/lib.rs:4327` (keyed by timerfd tag
  bits, 1080 woken / 10 s). Next lane: name the wake source of the unattributed bucket.
- `servers/net/src/lib.rs:7341` `unix_end_release` still bit-only (6 calls, 90 woken / 10 s here).
