# Lane ffpaint (2026-10-14/15): Firefox stops repainting during 1080p video (aarch64)

Branch `lane/ffpaint`, base main `9f4f1753`. Mac aarch64/HVF, virgl on
`~/.local/qemu-gpu-gles31` (patched virglrenderer 1.3.0, ANGLE -> MoltenVK),
guest RAM 4G, Firefox 136, `yt2.html` (1080p30 VP9 test pattern, 12 min,
looping) served from the Mac on vmnet.

## Status: root-caused, fix committed, NOT yet validated in a video run

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
  sites (anon, device, shared-vmo, file-private, eager file-copy). Logs the
  first 16 relocations as `[MMAP] range taken, placed elsewhere: ...` (each
  one is an mmap that used to fail with ENOMEM).
- `userland/memtest`: `mmap_skips_occupied_ranges` reserves 64 MiB right above
  the next unhinted address, then does 96 unhinted mmaps (anon, file-private,
  tmpfs shared) through it, plus a hint into the reservation.
- `ffsession.py`: `--shot-every`, `--freeze-detect N`, `--freeze-watch S`,
  `--freeze-cmd`, `FFSESSION_HOLD=<file>` (+ `<file>.cmd` channel),
  `--fresh-profile`.

Type-checked on aarch64 and x86_64; the aarch64 image with the fix built, but
the validation runs never started (a stale QEMU left by a harness crash blocked
them). **Not yet run: memtest, regress, video runs.**

## Next steps (in order)

1. Build both arches (`./scripts/build-all.sh`), run `runtests.py aarch64 t
   --suite regress` and x86_64; confirm `mmap_skips_occupied_ranges: PASS`.
   Check that the new test FAILS on main 9f4f1753 (expected: "an unhinted mmap
   failed").
2. Fixed-tree video runs, fresh profile: `run.sh aarch64 K<n> 600
   --fresh-profile` (from `lane-ffpaint-files/`, set WT; needs `rangeserver.py
   8775` in a dir holding `yt2.html` + `long1080.webm`, made with `ffmpeg -f
   lavfi -i testsrc2=size=1920x1080:rate=30 -f lavfi -i sine -t 720 -c:v
   libvpx-vp9 -b:v 4M -deadline realtime -cpu-used 8 -row-mt 1 -c:a libopus
   long1080.webm`). Done = 3+ consecutive clean runs, `scan.sh` shows wrOOM=0,
   and count `[MMAP] range taken` lines in serial.log (direct proof the fix
   caught overlaps that used to fail).
3. Strongest A/B: the old tree with only the fix applied, fresh profiles
   (old fresh rate ~1/3). `lane-ffpaint-files/old-tree-diagnostics.patch` is
   the diagnostics used there (thread names in Ctrl-T, late-wake detector,
   `DBG_ENOMEM`); `patch_trace.py` adds the ENOMEM/DRM-ioctl/mmap failure trace.
4. One x86_64 run (TCG, alone; or KVM on 172.16.158.150).
5. Follow-ups, not done: MAP_FIXED_NOREPLACE is not implemented (treated as a
   hint); `brk/refused vma-overlap` happens ~35x per boot (a bump mapping sits
   right above small heaps; musl falls back to mmap, so harmless); virgl
   RESOURCE_CREATE backing is one contiguous 2^order block.

## Harness pitfalls met

- macOS /tmp is case-insensitive: run tags `f1` and `F1` are the same dir.
  Moving a run dir while its session runs crashes ffsession at the end and
  leaves QEMU running, which fails every later boot with "QEMU already
  running". Use distinct tags; stop with `driver.py stop` under the run id.
- The hold channel's `sh()` loses commands on the old tree's driver.
- Worktrees: `.claude/worktrees/ffpaint` (this lane), `ffpaint-old` (detached
  7d114ce0 + diagnostics, uncommitted), `ffpaint-old2` (a `cp -c` clone of
  ffpaint-old, not a git worktree; delete with `rm -rf`). The old ones can be
  removed once step 3 is done or skipped.
