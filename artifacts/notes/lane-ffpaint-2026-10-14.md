# Lane ffpaint (2026-10-14/15): Firefox stops repainting during 1080p video (aarch64)

Branch `lane/ffpaint`, base main `9f4f1753`. Mac aarch64/HVF, virgl on
`~/.local/qemu-gpu-gles31` (patched virglrenderer 1.3.0, ANGLE -> MoltenVK),
guest RAM 4G, Firefox 136, `yt2.html` (1080p30 VP9 test pattern, 12 min,
looping) served from the Mac on vmnet.

## Status: root-caused, fixed, reviewed; regress green both arches; 7 video runs clean but the fix path was not hit in them (see Validation 2026-10-15)

### Root cause (kernel, still present on main 9f4f1753)

Non-`MAP_FIXED` mmap placed mappings without checking that the range was free.
`sys_mmap_inner` took the caller's hint, or `MMAP_BUMP.fetch_add(len)` (one
system-wide counter that only grows), and mapped there; if a VMA already
occupied any of it, the map failed with `vma-overlap` and the syscall returned
ENOMEM. Only the anonymous and file-private paths retried, and only for a
*hinted* mmap. Device (DRM BO), tmpfs/memfd shared and eager file-copy paths
never retried, and nothing retried an unhinted mmap.

Because the counter is global and monotonic it sweeps upward through every
process's address space. Firefox (SpiderMonkey, mozjemalloc) reserves ranges at
hinted, random addresses; once the counter reaches one, that process's
unhinted mmaps fail until the counter has passed it. Firefox's WebRender
"Renderer" thread maps virgl buffers with `mmap(NULL, ..., drm_fd, off)`: the
map fails -> Mesa raises GL_OUT_OF_MEMORY -> WebRender `wr_renderer_render:
OutOfMemory` -> device reset (about 35 s of renderer re-init + ~80 shader
recompiles through virgl) or "Fallback WR to SW-WR, forced" (painting stops
for good). The video keeps decoding, the dropped-frame counter freezes (no
compositing), the panel clock keeps ticking: the reported symptom exactly.

### Evidence

- Old tree run h2 (fresh profile, kernel with `DBG_ENOMEM` + thread names):
  serial `[ENOMEM] reason=vma-overlap [ENOMEM] mmap/device pid=562`; Ctrl-T dump:
  `pid=562 tgid=494 ... name=Renderer (/usr/lib/firefox/firefox)`. Firefox log:
  `wr_renderer_render: OutOfMemory (t=173.218)` then `Fallback WR to SW-WR,
  forced`; screenshots frozen from t~168 s for 120+ s; YT log: frames rising,
  dropped stuck at 547 from t=161 to t=322.
- Old tree run o1: `wr_renderer_render: OutOfMemory (t=401.579)` ->
  `Detect DeviceReset FORCED_RESET` -> 3 renderer re-inits; screen frozen
  t~399..435, then recovered.
- Every run with a freeze had WebRender OutOfMemory, and no run without a
  freeze had it (scanner: `lane-ffpaint-files/scan.sh`).
- Old tree run q1: content process `Call to mmap failed: Out of memory`
  (SharedMemory::Map) -> tab crash: same class (shared-vmo path).
- The current tree has identical placement code (verified by reading
  `sys_mmap_inner` on 9f4f1753), so the bug is latent on main; it was simply
  not hit in the runs below.

### Ruled out

- Lost poll/epoll wakeup: a late-wake detector (timed poll/epoll that slept to
  its deadline while an fd was ready) logged 0 events on the current tree and
  2 marginal ones (~1 s polls) on the old tree; unix socket rings were empty in
  the freeze dumps. Not the cause.
- Firefox WaylandProxy 30 s poll timeout (my first hypothesis for o1's ~35 s
  stall): the recovery matched WebRender re-init, not the proxy.
- Buddy fragmentation of contiguous virgl backing (RESOURCE_CREATE allocates
  2^order contiguous pages): no `[BUDDY] Allocation failed` in any run. Still
  a latent weakness worth fixing separately (Linux uses scatter-gather).
- Host GPU (ytfreeze class): no GPU errors, no HOST GPU HUNG.
- x86_64/TCG run x1 had 20-40 s static stretches, but the dropped counter kept
  rising and there was no OutOfMemory: TCG slowness, not this bug.

## Repro rate (10-min runs, screenshot detector: 3 static shots 10 s apart)

| tree | profile | runs | freezes |
|---|---|---|---|
| current 9f4f1753 (a1-a7, c1, gA1-2, G1) | reused/fresh | 7 reused + 5 fresh | 0 |
| x86_64/TCG current (x1) | fresh | 1 | 0 real (TCG stalls only) |
| old 7d114ce0 (o1-o5, on1-on7, q1, p1-p6, fA1-2, F1, hA1-2, H1) | reused + fresh | 14 reused, 9 fresh | 2 (o1, h2), both fresh-profile; plus q1 content crash (fresh) |

A fresh profile (first run after an image build, or `--fresh-profile`) is the
trigger that matters: first-run JS work (Remote Settings sync, certificate
downloads) makes many hinted reservations. Old tree fresh: 2 freezes + 1 crash
in 9 runs; reused profiles: 0 in 14. Current tree fresh: 0 in 5 (not enough to
say the rate differs; the bug is in the code either way).

## Fix (commit on lane/ffpaint)

- `mm/src/vmm.rs`: `AddressSpace::overlap_end(virt, len)`.
- `kernel/src/syscall.rs`: `mmap_place()` picks the hint or bump candidate if
  free, else lifts `MMAP_BUMP` past the obstacle and takes a fresh range;
  called inside the same address-space hold as the map, at all five non-FIXED
  sites (anon, device, shared-vmo, file-private, eager file-copy). Logs
  relocations as `[MMAP] range taken, placed elsewhere: n=N bump|hint ...`
  (see faf4cc54 below for which ones; each `bump` one is an mmap that used to
  fail with ENOMEM).
- `userland/memtest`: `mmap_skips_occupied_ranges` reserves 64 MiB right above
  the next unhinted address, then does 96 unhinted mmaps (anon, file-private,
  tmpfs shared) through it, plus a hint into the reservation.
- `ffsession.py`: `--shot-every`, `--freeze-detect N`, `--freeze-watch S`,
  `--freeze-cmd`, `FFSESSION_HOLD=<file>` (+ `<file>.cmd` channel),
  `--fresh-profile`.

Follow-up commits after review (2026-10-15):

- `92986f2d` a taken *hint* no longer lifts `MMAP_BUMP`. fccbcbd8 did
  `fetch_max(obstacle end)` for hints too, so one taken random 47-bit hint
  (SpiderMonkey/mozjemalloc) could teleport the system-wide counter to near
  the top of user space. Hint and bump relocations are counted separately.
- `f36d4fa5` MAP_FIXED_NOREPLACE implemented (exact or EEXIST, never unmaps,
  wins over MAP_FIXED, unaligned = EINVAL). It used to be a plain hint, and a
  relocated hint would have handed such a caller an address it did not ask
  for. memtest `mmap_fixed_noreplace`.
- `faf4cc54` the relocation log broke boots: greetd makes ~100 hinted mmaps
  that are taken (it hints just past its last mapping) while getty prints
  `login: `; the pieces of the log line (`serial_print_hex` writes "0x"
  unbuffered) interleaved with the prompt (`0lxo0gxi0xn: `) and
  `driver.py start` never saw it: 3 of 4 aarch64 boots failed. The line is now
  built whole; hint relocations log only at powers of two >= 1024; bump
  relocations (the ones that used to fail) log n<=16 and powers of two.
  5/5 boots after.
- `0fb6f020` ffsession stops QEMU when the boot fails (it used to leave it
  running, and the next run under the id failed "QEMU already running").

Review, no change needed: placement runs inside the same
`with_current_address_space_mut` hold as the map at all five non-FIXED sites;
the eager file-copy and shared-vmo sites rebind `virt` to the placed address
for the rest of their work; `overlap_end` uses the same overlap test as every
`map*` (brk heaps are VMAs too); `checked_add` + `USER_SPACE_END` bound the
walk. Linux searches for a free range when the hint is taken; taking the next
bump range is equivalent for callers. Log cap is fine after faf4cc54.

## Validation 2026-10-15 (lane head after the commits above)

- memtest: `mmap_skips_occupied_ranges` and `mmap_fixed_noreplace` PASS on
  both arches. The test on main 9f4f1753 would fail (reasoned, not run; the
  old-tree worktrees were deleted): the first unhinted 1 MiB anon mmap after
  the probe lands at probe+4 KiB and overlaps the reservation at
  probe+64 KiB; main's anon path retries only hinted mmaps, so it returns
  ENOMEM -> "an unhinted mmap failed". In every regress run the only bump
  relocation in the whole boot is this test's
  (`n=1 bump pid=157 wanted=0xe8dfcc000 at=0xe91fdb000 len=0x100000`).
- `runtests.py --suite regress` + ptytest + privtest: aarch64 22/22,
  x86_64 22/22 (run 3 times each across the commits: ra1-3, rx1-3).
- Video runs (`run.sh`, fresh profile, yt2.html 1080p30 VP9, out dir
  /tmp/ffpaint-runs4):

| run | arch | length | frozen | wrOOM | reset | ENOMEM | bump reloc | hint reloc |
|---|---|---|---|---|---|---|---|---|
| va1 | aarch64/HVF | 10 min | no | 0 | 0 | 0 | 0 | 1024-2047 |
| vb3 | aarch64/HVF | 10 min | no | 0 | 0 | 0 | 0 | 1024-2047 |
| vc1 | aarch64/HVF | 10 min | no | 0 | 0 | 0 | 0 | 1024-2047 |
| vc2 | aarch64/HVF | 10 min | no | 0 | 0 | 0 | 0 | 1024-2047 |
| vc3 | aarch64/HVF | 10 min | no | 0 | 0 | 0 | 0 | 1024-2047 |
| vd1 | aarch64/HVF | 30 min | no | 0 | 0 | 0 | 0 | 1024-2047 |
| vx1 | x86_64/TCG | 10 min | no | 0 | 0 | 0 | 0 | 1024-2047 |

  va1/vb3 ran on the 92986f2d kernel (log not yet fixed; va2/va3/vb2 are the
  boot failures that led to faf4cc54), vc*/vd1/vx1 on faf4cc54. vx1 dropped
  14549 of 30774 frames: TCG, not a freeze (counter kept rising).

**Verdict on the evidence: weak for "the fix stops the freeze", strong for "the
fix is safe".** No bump relocation happened in any Firefox run, so the code
path that turns the old ENOMEM into a relocation was never hit in Firefox
(the trigger is rare: a reservation lying exactly in the counter's path; the
old tree hit it in 2-3 of 9 fresh-profile runs; the current tree froze in 0
of 5 fresh runs before the fix and relocated a bump range in 0 of 7 after). Seven clean runs neither prove nor
disprove the fix for Firefox. What proves the mechanism is memtest, which
reproduces it deterministically (an unhinted mmap through a hinted
reservation) on the anon, file-private and shared paths; the device path
uses the same `mmap_place`.

## Next steps

1. Optional stronger A/B: old tree 7d114ce0 + fccbcbd8..faf4cc54, fresh
   profiles (old fresh rate ~1/3); expect relocations with `bump` instead of
   `[ENOMEM] mmap/device` and no freeze. `old-tree-diagnostics.patch` and
   `patch_trace.py` are the diagnostics used there.
2. Follow-ups, not done: `MMAP_BUMP` is one global counter that never reuses
   space and never wraps; the 30-min run moved it to ~0x66_xxxx_xxxx
   (~410 GiB), so 128 TiB lasts days of video, not forever (a per-process
   next-fit search, or a wrap back to the base now that placement checks
   overlap, would fix it). mremap with MREMAP_FIXED maps its target as a hint
   without unmapping (not Linux). `brk/refused vma-overlap` ~35x per boot
   (harmless, musl falls back to mmap). virgl RESOURCE_CREATE backing is one
   contiguous 2^order block. `serial_print_hex` writes "0x" unbuffered ahead
   of the buffered digits, so every kernel line using it is split.

## Harness pitfalls met

- macOS /tmp is case-insensitive: run tags `f1` and `F1` are the same dir.
  Moving a run dir while its session runs crashes ffsession at the end and
  leaves QEMU running, which fails every later boot with "QEMU already
  running". Use distinct tags; stop with `driver.py stop` under the run id.
- The hold channel's `sh()` loses commands on the old tree's driver.
- Worktrees: `.claude/worktrees/ffpaint` (this lane). The old-tree ones
  (`ffpaint-old`, `ffpaint-old2`) were deleted on 2026-10-15.
- `run.sh` now uses run id `ffpaint` / VNC 5972 (it used `ffpaintc` / 5975,
  which collided with another lane's port) and `$FFPAINT_OUT`
  (default /tmp/ffpaint-runs4); `scan.sh [DIR]` also counts relocations.
