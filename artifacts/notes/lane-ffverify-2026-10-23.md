# lane/ffverify — 2026-10-23 (openbugs L11.3)

Mac aarch64/HVF, `--virgl`, release build of main `d9476afd`, guest RAM 2G, Firefox 136.
No kernel change: nothing in L11.3 reproduced as a kernel bug in QEMU. Tooling only.

## Setup (reproducible)

- Page `lane-ffverify-files/ffv.html`: a 720p30 VP9+Opus test-pattern video (120 s, restarted by
  JS on `ended`/`error`/stall, because a plain `loop` hit `NS_ERROR_DOM_MEDIA_DEMUXER_ERR` at the
  end of the file in run v1), an rAF frame counter, and a 300-row page auto-scrolled every 100 ms.
  The page dumps `YT t= ct= frames= dropped= raf=` every 10 s to ff.log.
  Video: `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 120 -c:v libvpx-vp9 -b:v 1500k -deadline realtime -cpu-used 8 -c:a libopus -b:a 64k v720.webm`,
  served from the Mac with `lane-ffpaint-files/rangeserver.py 8776` (host-local; no internet).
- `ffsession.py aarch64 <tag> --url http://192.168.105.1:8776/ffv.html --put ffm.sh:/tmp/ffm.sh --post "sh /tmp/ffm.sh" --post-timeout 480`
  (`--post-timeout` is new in this lane; `--post` was capped at 120 s).
- `lane-ffverify-files/ffm.sh` (guest, after the observation, Firefox still playing): `/proc/sysstat`
  snapshots of every Firefox process (`t` per-thread CPU ns, `s` per-syscall counts) at
  on0 → +60 s on1 → `blanktest --off` (zwlr_output_power off) → +10 s off0 → +110 s off1 →
  `blanktest --on` → +10 s back0 → +50 s back1. The guest has no grep/awk; the filter is a
  `while read; case` loop. `an.py post.txt` prints per-process CPU, top syscalls/s and top threads.

## (a) paint-freeze fix exercised by Firefox — VERIFIED (path changed since ffpaint)

The ffpaint trigger (global monotonic `MMAP_BUMP` counter running into a hinted reservation) no
longer exists: lane/vmreuse2 (`fe5126c6`, 2026-10-19) replaced it, and every non-fixed mmap now goes
through `mmap_place` → `find_free_range` (lowest free range of the caller's own address space)
whenever there is no hint or the hint is taken. So every unhinted Firefox mmap runs the fixed code.

| run | observed | shots changed (frac of pixels per 20 s shot) | WR OutOfMemory / DeviceReset / SW fallback | `[MMAP]` hint relocations logged |
|---|---|---|---|---|
| v1 | 840 s | 29/29 shots changed, 0.695–0.699 | 0 | n=1024, 2048, 4096, 8192 |
| v2 | 780 s after page load | 25/25 after load, 0.691–0.705 | 0 | 4 lines (to 8192) |
| v4 | 300 s | — | 0 | 4 lines |

- Firefox mmap traffic in v4 (cumulative, `/proc/sysstat` `s` nr 222): parent 16 815, Web Content 1 724,
  Privileged Content 869, RDD 642, WebExtensions 632, Utility 493, Socket 338 — 21 513 mmaps, 0
  failures visible to Firefox (no `Call to mmap failed`, no WebRender OOM).
- The taken-hint relocation counter reached 8192 in a 15-min Firefox run (the comment in
  `kernel/src/syscall.rs` puts the boot baseline at ~100): Firefox drives the relocation path heavily.
- rAF counter kept rising ~60/s with the screen on in every run (v1: 50 194 frames in 840 s).
- v2's freeze detector fired at t=21 s: the first 4 shots were the loading page; the first HTTP
  request to the host took ~120 s in that run (10 s in v1/v3/v4). Not a paint freeze; slow first
  connect seen once (not investigated).

## (b) Web Content spin after blank — NOT REPRODUCED in QEMU (REFUTED for this setup)

v4, video playing throughout (ct kept advancing while blanked, `frames` rising), per-process CPU
from summed thread `cpu_ns`:

| process | on (61 s) | blanked (111 s) | back on (52 s) |
|---|---|---|---|
| firefox (parent) | 37.4 % (Renderer 19 %, ioctl 15 % of a CPU) | 3.0 % | 39.0 % |
| Web Content | 11.5 % | 3.0 % | 11.4 % |
| RDD | 0.2 % | 0.1 % | 0.1 % |

- Blanked, Web Content's syscalls: clock_gettime 4677/s, futex 2095/s (half return 0 = wake with no
  waiter), recvmsg 338/s, epoll_pwait 214/s with **0 zero returns** (no spurious/timeout wakes; no
  readiness that never clears). rAF drops to ~1/s (cosmic-comp's DPMS-off frame-callback timer).
- So blanking lowers Firefox's CPU 4–12×, as it should. The ~3-core spin was a laptop/YouTube
  observation (i915, VA-API); blankplay's own laptop table shows Web Content 28–77 % blanked, not 3
  cores. Still unverified on the real laptop with this probe; `ffm.sh` runs there unchanged as root.

## (c) RDD poll spin — REFUTED as a kernel bug (legitimate Firefox behaviour)

- Already root-caused on the laptop in `rddspin-2026-10-19.md`: every RDD poll is
  `DMABufSurface::IsGlobalRefSet()` (nfds=1, timeout 0, eventfd) over the VA-API frame pool; the
  kernel's answers are correct; the rate scales with pool size (17–150 surfaces).
- QEMU has no VA-API, so the loop does not exist here: RDD made no ppoll calls worth listing
  (top: clock_gettime 275/s, futex 177/s) and used 0.1–0.2 % CPU on, blanked and back on.

## Changes

- `.claude/skills/run-leandros/ffsession.py`: `--post-timeout S` (default 120).
- `artifacts/notes/lane-ffverify-files/`: `ffv.html`, `ffm.sh`, `an.py`.

## Open / new

- Every Firefox process shows exactly 2048 or 4096 mremap calls (sysstat nr 216) — a
  power-of-two count in five processes looks like a fixed retry loop at startup; worth a look
  (repro: v4 `post.txt`, `s <tgid> 216`).
- Parent Renderer thread does ~7.8k sched_yield/s and ~2k virgl ioctl/s with the screen on (19 %
  CPU): GPU fence busy-wait in Mesa/virgl, not blank-related.
- (b) needs one laptop run with `ffm.sh` during YouTube with the panel idle-blanked to close it
  on hardware.

## Regress

aarch64 `runtests.py` regress on this lane (tooling-only diff): all suites green except
`nettest tcp_keepalive_probes` and `netctltest wired_admin_up_rebinds` (1 fail each); both passed on
an immediate re-run under the lock (`ffverify-net`, PASS). New flakes to add to the known list.
