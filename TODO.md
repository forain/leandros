# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`531f21e`), now also covering the
m9 panel-gate re-measurement wave.

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment *unmodified* (source: `../cosmic-epoch`)
on both x86_64 and aarch64 under QEMU. No COSMIC source patches; build-configuration
flags (`--no-default-features`) are allowed. Everything beneath COSMIC — kernel, libc,
system libraries, daemons — is ours.

**Where it stands.** The desktop runs on both arches: cosmic-session → cosmic-comp on
KMS/softpipe → busd → cosmic-bg + cosmic-panel renders a wallpaper plus a full-width
panel bar with an embedded Wayland client. Remaining work is quality and performance,
not bring-up. The full suite is green on freshly-built release binaries and fresh
images, both arches, as of 2026-08-06: scmtest 25/0, vfstest 36/0, drmsmoke 22/0,
wakepoll/fork/epoll/poll/sig/timer/mem all FAIL=0, waittest aarch64 3/2 (the known
`wait_on_process_group` failure, byte-identical to the 2026-08-05 baseline) and x86_64
5/0; `xattr_list_f2fs` was absent on aarch64 on a fresh image, confirming the
dirty-image theory.

**Committed architecture** (settled; revisit only with a reason):

- COSMIC builds for `*-unknown-linux-musl`, **dynamically linked** against a real
  `ld-musl`. No Rust std port. `dlopen` is on the critical path in three places
  (cosmic-comp's EGL loading, Mesa's GBM/DRI loader, cosmic-panel's `use_system_lib`).
- Graphics: Mesa **softpipe** via gallium `kms_swrast` over dumb buffers. The atomic
  KMS path is live and preferred; the legacy path still works but cannot drive a
  cursor plane.
- Seat/input: **shim** libseat and libudev; **port** real libinput and libxkbcommon.
  No seatd, no udevd, no VT switching.
- D-Bus: **busd** (pure Rust, from the zbus authors). Reference `dbus-daemon` is the
  fallback if it proves immature.
- `start-cosmic` runs under **brush**; boot path is login → root → `start-cosmic`.

**Load-bearing session env** (in the launcher at
`~/code/leandros-artifacts/m6-session-data/start-cosmic-leandros`, not in this repo):

- `COSMIC_RENDER_DEVICE=226:0` — card0's dev id. Without it, cosmic-comp's
  `determine_primary_gpu` filters our software device out and falls back to an EGL
  display that lacks `EGL_EXT_image_dma_buf_import`.
- `COSMIC_DISABLE_OVERLAY_SCANOUT=1` — **never** `COSMIC_DISABLE_DIRECT_SCANOUT=1`.
  smithay's `FrameFlags::ALLOW_SCANOUT` is a union that includes
  `ALLOW_CURSOR_PLANE_SCANOUT`, so the latter silently disables the cursor plane with
  no log line at any level.
- `SMITHAY_USE_LEGACY` must stay unset so smithay takes the atomic path.

**Kernel invariants.**

- Never touch user memory under `RUN_QUEUE` or any IRQ-off spinlock. Use
  `validate_user_buf`/`read_user_buf`/`write_user_buf`. A re-entrant `RUN_QUEUE`
  deadlock from exactly this froze all four vCPUs once (fixed in `82d0cc3`).
- **The kernel is softfloat on both arches and must stay that way.** The EL0 trap
  frame saves no vector state, so any kernel code LLVM lowers through a vector
  register lands on the interrupted thread's. Both kernel target JSONs disable the
  vector units; `cpu_switch_to` is the single deliberate exception and scopes the
  extension with `.arch armv8-a+fp+simd` … `.arch armv8-a`.
- Release builds only — debug builds crash early. Test **both** arches in QEMU after
  every change. Minimum Limine revision is **6**, never downgrade.
- Regression images must be freshly regenerated; a dirty f2fs image produces
  phantom failures (classically `xattr_list_f2fs`).

**Diagnostics in-tree, all compiled out by default** — flip to `true`, measure,
flip back before committing:

| Flag | File | Measures |
|---|---|---|
| `DRM_STATS` | `drivers/src/drm_device_interface.rs:1230` | flips, cursor up/mv, atomic, atest, cplane |
| `CURSOR_DEBUG` | `drivers/src/virtio_gpu.rs:342` | cursor queue setup + selftest |
| `mm::gap2::ON` | `mm/src/gap2.rs:17` | memfd/MAP_SHARED path + frame checksum sampler |

**Evidence lives outside this repo.** Run logs, screenshots, research notes and test
harnesses are in `~/code/leandros-artifacts/notes/`. Design docs that are still
execution-ready are in `docs/design/`.

**Explicitly out of scope** (all degrade gracefully or are non-fatal): XWayland,
PipeWire/audio for COSMIC, NetworkManager, UPower, accountsservice, greetd +
cosmic-greeter, cosmic-workspaces' wgpu path, hotplug, VT switching, multi-seat.

---

## Open work

| # | Item | Category | Blocked on |
|---|---|---|---|
| 1 | Venus/virgl — round-trip done on x86_64; vktest hangs under TCG | Feature | — |
| 2 | cosmic-panel bar frozen at first frame | Bug — compositor-side | — |
| 3 | Nested epoll fd may report readable unconditionally | Bug — kernel | — |
| 4 | memfd burns a tmpfs slot per call | Bug — latent DoS | — |
| 5 | `wl_display error 0 "Unknown id: 636"` | Bug | re-measure post-fix |
| 6 | `FB_DAMAGE_CLIPS` / primary-plane recomposite | Perf | — |
| 7 | evdev monotonic timestamps (reverted) | Bug | needs a different time source |
| 8 | Doom hangs in `malloc(16 MB)` on aarch64 | Bug | re-verify first |
| 9 | AF_INET loopback `bind()` → EINVAL | Bug | — |
| 10 | Deferred / known limitations | Mixed | — |

---

### 1. Venus/virgl — round-trip done on x86_64; vktest hangs under TCG

The host round-trip works. On the Linux box (`forain@172.16.158.150`, EndeavourOS,
virglrenderer 1.3.0, QEMU 11.0.1 — already installed, nothing to add; note it is
**Arch, not Debian**, so the old `apt install` line was wrong), on softfloat HEAD with
fresh images: x86_64/KVM gives `venustest` **68/68** and `vktest` **0 failures**,
opening a real GPU through Mesa's Venus ICD (`Virtio-GPU Venus (AMD Ryzen 9 7950X (RADV
RAPHAEL_MENDOCINO))`, `vkCreateDevice` VK_SUCCESS); aarch64/TCG gives `venustest`
**68/68**, `drmsmoke` 22/22, `vfstest` 36/36 — the first-ever aarch64 Venus run,
transport layer fully green.

Venus needs the device line `-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G
-display egl-headless`. `scripts/run-qemu.sh` does **not** pass these, and on x86_64 it
selects `virtio-vga` (no GL at all), so the in-tree harness cannot exercise Venus — only
bespoke wave scripts can. Worth fixing. Reminder: `-nographic` silently overrides
`-display`.

**Open:** `vktest` hangs at `vkEnumeratePhysicalDevices` (after `vkCreateInstance`
succeeds) under **TCG on both arches**. x86_64/KVM passes; x86_64 under `-accel tcg`
reproduces the hang identically with the same `[DRM]` trace — so it is neither
arch-specific nor softfloat-related. The guest stays healthy: Ctrl-C returns to the
shell, all vCPUs idle in the kernel, QEMU ~4.6% CPU, no host renderer error — a thread
blocked in a wait, not spinning or crashed. Leading hypothesis: the GPU has no ISR, so
completions are polled; under KVM the poll wins the race, under TCG the reply is never
observed. Next diagnostic: instrument the ring-reply/fence wait path and check whether
the host renderer was ever notified for that submission.

Because an aarch64 guest on an x86_64 host is TCG-only, **aarch64 Vulkan remains
unvalidated — untested, not broken.** It needs this hang fixed, or an ARM host.

The old "`venustest` fails 29 / `host lacks VIRGL/BLOB/CONTEXT_INIT`" line was a
**macOS-host** artifact (no EGL) — not a code defect, and not the state on Linux.
macOS-has-no-EGL and rutabaga-is-a-dead-end both remain accurate.

`vkcube` is **not** a runnable follow-on: it has never been built for LeandrOS (no
binary or source in the repo or in `leandros-artifacts`;
`scripts/mkfs-f2fs-populated.py` stages only `vktest` + `libvulkan_virtio.so`), it links
the Khronos `libvulkan.so.1` loader that we deliberately do not ship (`vktest` exists
precisely to bypass the loader and `dlopen` the ICD directly), and no WSI has been
chosen among the ICD's `VK_KHR_wayland_surface` / `VK_KHR_display` /
`VK_EXT_headless_surface` / `VK_EXT_acquire_drm_display`. That is the M3 rendering
milestone.

### 2. cosmic-panel bar frozen at first frame

The re-measurement on the fixed softfloat kernel is done. **The clock is still
frozen** — `05f7279` did not fix it. Three independent runs on aarch64/HVF with fresh
release builds and fresh images: clock reads `00:00:08` at t=70 s and t=175 s with the
bar strip byte-identical, `00:00:10` at t=100/118/170 s across 542 pointer moves, and
`00:00:08` at t=65 s and t=190 s across 631 pointer moves. Evidence in
`~/code/leandros-artifacts/notes/m9-panelgate/` (`clock-m9a-t70.png`,
`clock-m9a-t175.png`, `clock-m9g-t65.png`, `clock-m9g-t190.png`).

The GAP2 observation survives re-taking on the fixed kernel, so the kernel is
genuinely exonerated this time: 887 samples over ~205 s show the applet pool
(`idx=0x14`, `/tmp/memfd:leandros-applet`, 220x32x4) advancing continuously with 150
transitions, while the panel's own bar pool (`idx=0x18`, 1280x32x4) is
**byte-constant across all 295 samples**. Log:
`notes/m9-panelgate/m9g-aarch64-g2lines.txt`.

The three candidates this item previously listed — smithay caching the imported SHM
texture, the compositor mapping the pool at a mismatched size/offset, and
`wl_shm_pool.create_buffer` offset handling — are all **ruled out**. The panel never
reaches the import path.

Root cause, measured: cosmic-panel's own **size gate** at
`cosmic-panel-bin/src/space/render.rs:104-109` (`actual_size.w/h <= 20 ||
dimensions.h <= 20`). Instrumented telemetry over ~789,000 `render()` calls in ~170 s
reported `actual_size=8x8 dims=1280x8 is_dirty=true has_frame=true` on every single
sample — both the dirty gate and the frame-callback gate are **open**, and the size
gate early-returns forever. Frame callbacks from cosmic-comp are therefore not the
problem, and applet commits are being delivered. 8x8 is padding-only, meaning **no
applet window is mapped in that space**, even though the applet process is alive and
painting (its pool advances ~1/s). This is the same gate that caused the M7w bring-up
failure, when `actual_size` was `(0,0)`.

Two caveats, **inferred not measured**, both being chased now. First, the telemetry
counter was per-site, not per-space; cosmic-panel runs several `PanelSpace`s in one
process and one of them spins fast enough to drown the others out of the sample, so
this proves *a* space is permanently gated at 8x8 but not yet that the **bar-owning**
space is. Second, `event_loop.dispatch(100 ms)` returning ~4,600 times per second
means calloop always finds a ready source and something is never drained — see the
new item on nested epoll readiness.

Diagnostic tooling, currently reverted but re-appliable:
`~/code/leandros-artifacts/m9_apply_panel_diag.py [--revert]` patches the shipped
build tree `~/code/leandros-artifacts/m6-session-bins/src/cosmic-panel` — **never**
`~/code/cosmic-epoch`, since no COSMIC source patch may ship. Harnesses
`m9_panelgate.py`, `m9_gap2.py`, `m9_uck.py` alongside it.

### 3. Nested epoll fd may report readable unconditionally

cosmic-panel's `event_loop.dispatch(100 ms)` returns ~4,600 times per second, so
calloop always finds a ready source and something is never drained. The structural
suspect is that the embedded wayland-server's `poll_fd()` is itself an **epoll fd
nested inside calloop's epoll** (`wayland-backend-0.3.x/src/rs/server_impl/common_poll.rs:37`).
On Linux an epoll fd is readable only when its own interest list has at least one
ready event; if our kernel reports a nested epoll fd as unconditionally readable,
then any calloop- or libevent-style consumer that nests epoll will busy-spin. Not yet
confirmed against our epoll implementation — measured symptom, inferred cause. If
confirmed it is a real kernel bug well beyond this panel, and wants a regression
subtest in `epolltest`.

### 4. memfd burns a tmpfs slot per call

`sys_memfd_create` backs each memfd with a *named* `/tmp/memfd:<name>` tmpfs node it
never unlinks, so every call permanently consumes one of 128 `MAX_TMP_FILES` slots. A
1 Hz repainter bricks `memfd_create` system-wide after ~100 frames.

The in-code comment (added in `b3659fa`) claims unlinking breaks things because
`ftruncate`/`mmap` still resolve the inode by name. **The audit contradicts that**: an
exhaustive site table (`notes/m8-research/gap1-byname-audit.md`) found every relevant
site is idx-keyed — `handle_ftruncate`, the K1 shared-VMO mmap path via
`tmpfile_owner_of`, `mark_memfd`, read/write/lseek/close/seals/`f*` xattrs. And
`36f62d0` already ships the same create-then-unlink-while-fd-open idiom for PRIME
export nodes.

Either the comment is stale or there is a runtime-only hazard the read-only audit
cannot see. Next: instrumented runtime re-test of unlink-after-create, then delete the
comment or record the real root cause.

### 5. `wl_display error 0 "Unknown id: 636"` — panel↔comp desync

Signature reads as one whole message dropped on a boundary. Id 636 is high and
client-allocated, created after globals + layer-surface + the whole EGL/GLES bring-up —
most likely a Mesa swrast `wl_shm_pool` created by the fd-carrying
`wl_shm.create_pool`, or its `wl_buffer`/`wl_callback` neighbour. That makes the
**SCM_RIGHTS branch** of `handle_sendmsg`/`handle_recvmsg` the suspect path, not the
plain-data branch. Full analysis: `notes/wl-id636-analysis.md`.

Observed on aarch64 with the FP/SIMD clobber live — "one whole message dropped
on a boundary" is also what silently corrupted userspace arithmetic looks like. Confirm
it still reproduces on the fixed kernel before spending time in the AF_UNIX path. The
scmtest hang turned out to be a host-side capture artifact, so the inference that this
is the same bug is void. On the current kernel, scmtest's `fd_pass`, `cmsg_flags`,
`shared_memfd_pixels`, `queued_fd_cap` and `full_ring_eagain` subtests all PASS on both
arches — evidence *for* the AF_UNIX SCM_RIGHTS path being healthy, so if 636 still
reproduces, look elsewhere first.

### 6. `FB_DAMAGE_CLIPS` / primary-plane recomposite

The cursor plane landed and moved pointer motion from **0.9 → 6.0 page flips/s**, with
the cursor image uploaded exactly once and zero pixel traffic per move. But the honest
caveat from that measurement is `flips/s == atomic/s == cursor_mv/s`: smithay still
flips the **primary** plane on every cursor frame. The end state
(`compositor/mod.rs:2318` "skipping primary plane, no damage") was not reached.

This is the remaining pointer-latency win, and it is on the primary plane, not the
cursor. `FB_DAMAGE_CLIPS` is already advertised in the plane property table.

### 7. evdev monotonic timestamps — reverted, do not re-land naively

Timestamping `push_event` from an inlined `monotonic_us()` **broke pointer input
entirely**: three runs with the change (atomic path, and a legacy-path control on the
identical build) all showed zero compositor response to 1000+ delivered pointer moves;
reverting it restored input. libinput evidently rejects the `cntvct`-derived
timestamps. A re-land needs a time source libinput accepts, verified against 60
moves/s with `DRM_STATS` on.

### 8. Doom hangs in `malloc(16 MB)` on aarch64

Doom runs through `DG_Init`, DRM init, a successful GPU flush and into the engine, then
hangs in the first `malloc(16 MB)` (`Z_Init` → `I_ZoneBase` → `AutoAllocMemory` in
`doomgeneric/i_system.c`). The `"zone memory: ... allocated"` print never appears.
x86_64 renders 1580 frames fine.

Diagnosed 2026-06-29: not a page-fault loop (<256 demand faults total, none while
hung — the 16 MB is never touched), and the kernel's `sys_mmap` anonymous path uses
`map_lazy` so it returns quickly. The hang is in relibc's dlmalloc or its `brk`/`mmap`
syscall glue.

**Re-verify before investigating.** That diagnosis predates `04c80cd` ("give relibc's C
sources a cross compiler, not the host one"), which touches exactly the layer blamed
here — this may already be fixed.

### 9. AF_INET loopback `bind()` → EINVAL

Found by the tokio spike: TCP loopback bind fails, so the tokio TCP subtest is skipped
while UDS passes. Low priority — Wayland and D-Bus need only UDS — but it is a real gap
in the smoltcp integration.

### 10. Deferred work and known limitations

- **Mesa modifier support.** Our GBM has no `gbm_bo_create_with_modifiers2` path, so
  smithay cannot build a reusing swapchain and reallocates per frame; this once burned
  128 dmabuf fds in ~1 s. `MAX_FDS` was raised 64→128 to absorb it. Revisit with
  PRIME/linux-dmabuf.
- **llvmpipe** — the TCG-performance lever, staged but not landed. softpipe was chosen
  for correctness (portable C, no per-arch LLVM codegen bring-up ×2).
- **Synthetic sysfs** — the read-only `/sys/dev/char`, `/sys/class/drm`,
  `/sys/class/input` design in `docs/design/k4-drm-design.md` is execution-ready but
  deferred; no current consumer needs the enumeration. (The PCI attributes the Venus
  render node needs were added separately.)
- **DRM ioctl gaps cosmic-comp tolerates** (kernel returns Unsupported): `VRR_ENABLED`
  property, syncobj. Nothing optional is advertised in the property table on purpose —
  smithay guards each and degrades cleanly.
- **ELF loader follow-ups from the dynamic-linking wave**: interp is eagerly loaded
  (~4.8 MB per exec), and there is a pre-existing buddy-slack leak on the eager→lazy
  split.
- **`/proc/self/exe` returns `/bin/init`** regardless of the caller.
- **libseat shim eventfd workaround** (`0bed5ad`) is inert now that the kernel honours
  `EFD_NONBLOCK`, and can be simplified.

---

## Housekeeping

- Untracked disk-image backups at the repo root
  (`f2fs-data0-aarch64.img.12h15-orig`, `.full-rebuild`, `.m7z2-orig-backup`,
  `f2fs-data0-x86_64.img.m7z2bak`) and `ports/busd/.work/` are now gitignored
  (`f2fs-data0-*.img.*`, `ports/*/.work/`); delete them by hand when no longer needed.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at
  exit 144.
