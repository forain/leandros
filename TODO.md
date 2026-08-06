# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`5cf1cb8`, plus the uncommitted
aarch64 FP/SIMD fix in item 1).

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
not bring-up.

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
  extension with `.arch armv8-a+fp+simd` … `.arch armv8-a`. See item 1.
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
| 1 | aarch64 kernel clobbered userspace FP/SIMD | Bug — kernel | fixed, **uncommitted** |
| 2 | `scmtest` hangs after ~2 subtests | Bug — regression | — |
| 3 | Venus/virgl host round-trip | Feature | a Linux host with virglrenderer |
| 4 | cosmic-panel bar frozen at first frame | Bug — compositor-side | re-measure post-item-1 |
| 5 | memfd burns a tmpfs slot per call | Bug — latent DoS | — |
| 6 | `wl_display error 0 "Unknown id: 636"` | Bug | re-measure post-item-1 |
| 7 | `FB_DAMAGE_CLIPS` / primary-plane recomposite | Perf | — |
| 8 | evdev monotonic timestamps (reverted) | Bug | needs a different time source |
| 9 | Doom hangs in `malloc(16 MB)` on aarch64 | Bug | re-verify first |
| 10 | AF_INET loopback `bind()` → EINVAL | Bug | — |
| 11 | Deferred / known limitations | Mixed | — |

---

### 1. aarch64 kernel clobbered userspace FP/SIMD — fixed, awaiting commit

Root-caused 2026-08-05, from the `cosmic-files-applet` crash loop
(`Error: Address("unix: address is invalid")` for a *valid*
`unix:path=/run/user/0/bus`, aarch64 only — x86_64 parsed the same string fine).

`targets/aarch64-unknown-kernel.json` built the kernel `+neon,+fp-armv8`, so LLVM
lowered kernel bulk copies through q registers, while
`arch/aarch64/src/exception_asm.s` saves a 288-byte EL0 trap frame (x0-x30, sp_el0,
elr, spsr, ttbr0) with **no vector state**. The file-backed demand-paging path
(`mm/src/vmm.rs`, gathered `FAULT_AROUND_PAGES=16` = 64 KiB f2fs read +
`copy_nonoverlapping` + cache maintenance) therefore overwrote the faulting thread's
`q0` and returned to EL0. Measured **960/15360 = exactly 1/16** page touches — one per
real gathered read — clobbered value `0xffff0000bc2b0000…`, a kernel HHDM pointer.
x86_64 was immune because its kernel target was already softfloat; a pure arch
asymmetry.

A fault fires on the first touch of any page, including the first execution of a
function, so the *first* SipHash in a process differed from every later one
(`hash pre=0xbff8… post=0xfd31… stable=false distinct_of_64=1`). A HashMap stored an
entry under an unreachable hash: `iter()` showed it, `get()` returned `None`, `insert`
missed duplicates. Blast radius scales with cold pages — constant in a 52 MB binary,
near-zero in a small one, which is why it presented as one broken applet.

Fix (two files, **uncommitted**): kernel target → `"+v8a,-neon,-fp-armv8"` +
`"abi"`/`"rustc-abi": "softfloat"` (mirrors rustc's own
`aarch64-unknown-none-softfloat`, and Linux's `-mgeneral-regs-only`);
`sched/src/context.rs` brackets `cpu_switch_to`'s `global_asm!` with
`.arch armv8-a+fp+simd` … `.arch armv8-a`, since it legitimately saves and restores
userspace q0-q31.

Verified: clobber 960/15360 → **0/15360**; applet parses every address form; aarch64
vfstest 36/36 fresh, and wakepolltest/epolltest/forktest/polltest/sigtest/timertest/
memtest/waittest **byte-identical to a baseline taken on the pre-change kernel**;
x86_64 rebuilt and re-tested green.

Notes for whoever picks this up: SVC, timer-IRQ and *anonymous* fault paths all
measured clean before the fix — only the file-backed path clobbered, so a syscall-only
test exonerates the kernel wrongly. The x86_64 `CpuContext.fpu` comment already records
the *context-switch* version of this same bug (found via MAME's SSE memcpy); this was
the trap-path analogue. Reproducer: `~/code/leandros-artifacts/hashprobe/`
(`roundtrip_fault` loads q0-q31, touches a cold page inside one asm block, stores and
diffs; the file-backed case needs the large `include_bytes!` blob so touches land in
exec-image VMAs). Delete this item once committed.

### 2. `scmtest` hangs after ~2 subtests

`scmtest` stops after `fd_pass: PASS` and never reaches the prompt; the recorded
baseline is 25/25. Reproduced on a fresh image, and **identically on a kernel built
before the item-1 fix**, so it is pre-existing and not caused by that change — but it
is a real regression against the recorded baseline and nothing else in the suite shows
it (vfstest 36/36, everything else matches baseline).

Bisect against the commits since the last green scmtest run; `servers/net`'s AF_UNIX
SCM_RIGHTS path is the obvious first suspect given what the test covers, and it is the
same code implicated in item 6.

### 3. Venus/virgl host round-trip

The kernel and DRM side is code-complete: the M1 transport (`04bde83`), the render node
plus the sysfs PCI attributes libdrm actually reads (`d5410ee`), the wire-protocol
corrections (`3598e80`, `bdab516`, `0dfc362`), and per-open/per-BO fences with scoped
GEM handles (`b80ab5a`).

It has never had a real host round-trip. Today's `venustest` run (aarch64) fails 29
subtests, all of them `[GPU] ctx_create refused: host lacks VIRGL/BLOB/CONTEXT_INIT` —
a host-capability wall, not a code defect. `virtio-gpu-gl-pci,venus=on` needs libepoxy's
EGL dispatch, and **macOS has no native EGL**; rutabaga is a confirmed dead end (its
device never sets the VIRGL/VENUS capset bits, checked v9.2 → master).

`5cf1cb8` made the QEMU harness run on a Linux x86_64 host, so the remaining gate is
environmental: on Linux, `apt install qemu-system-x86 libvirglrenderer-dev` gives a
working device with no source builds. Then re-run `venustest`, and after that `vkcube`.

### 4. cosmic-panel bar frozen at first frame

The panel renders a bar, but its clock is frozen at the first frame.

The kernel is exonerated. The gated GAP2 instrumentation (`345b84b`) showed the
applet's shared pool contents change once per second while the panel's own bar pools
stay byte-constant and the screen stays byte-identical over 105 s. Four hypotheses were
falsified by source reading before that measurement: the memfd MAP_SHARED path is
coherent cross-process (proven by `scmtest`'s pattern-B check), the applet's
attach/damage/commit ordering is correct, `time()` advances on aarch64 so the repaint
gate fires, and a read-only compositor mapping still takes the aliasing path.

**Re-measure before acting on any of this.** Every measurement above was taken on
aarch64 with the item-1 FP/SIMD clobber live, which silently corrupted userspace
computation in proportion to cold pages — exactly the conditions a freshly-started
compositor and panel run under. The conclusion may survive; the evidence has to be
re-taken first.

If it does survive, the bug is compositor-side. Remaining candidates:

1. smithay/cosmic-comp caching the imported SHM texture across frames despite a fresh
   `wl_buffer` and full-surface damage — check smithay's shm import and renderer
   texture-cache keying.
2. The compositor mapping the pool at a size/offset that does not track the client's.
3. `wl_shm_pool.create_buffer` offset handling.

### 5. memfd burns a tmpfs slot per call

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

### 6. `wl_display error 0 "Unknown id: 636"` — panel↔comp desync

Signature reads as one whole message dropped on a boundary. Id 636 is high and
client-allocated, created after globals + layer-surface + the whole EGL/GLES bring-up —
most likely a Mesa swrast `wl_shm_pool` created by the fd-carrying
`wl_shm.create_pool`, or its `wl_buffer`/`wl_callback` neighbour. That makes the
**SCM_RIGHTS branch** of `handle_sendmsg`/`handle_recvmsg` the suspect path, not the
plain-data branch. Full analysis: `notes/wl-id636-analysis.md`.

Observed on aarch64 with the item-1 FP/SIMD clobber live — "one whole message dropped
on a boundary" is also what silently corrupted userspace arithmetic looks like. Confirm
it still reproduces on the fixed kernel before spending time in the AF_UNIX path. If it
does, item 2 is probably the same bug and they should be chased together.

### 7. `FB_DAMAGE_CLIPS` / primary-plane recomposite

The cursor plane landed and moved pointer motion from **0.9 → 6.0 page flips/s**, with
the cursor image uploaded exactly once and zero pixel traffic per move. But the honest
caveat from that measurement is `flips/s == atomic/s == cursor_mv/s`: smithay still
flips the **primary** plane on every cursor frame. The end state
(`compositor/mod.rs:2318` "skipping primary plane, no damage") was not reached.

This is the remaining pointer-latency win, and it is on the primary plane, not the
cursor. `FB_DAMAGE_CLIPS` is already advertised in the plane property table.

### 8. evdev monotonic timestamps — reverted, do not re-land naively

Timestamping `push_event` from an inlined `monotonic_us()` **broke pointer input
entirely**: three runs with the change (atomic path, and a legacy-path control on the
identical build) all showed zero compositor response to 1000+ delivered pointer moves;
reverting it restored input. libinput evidently rejects the `cntvct`-derived
timestamps. A re-land needs a time source libinput accepts, verified against 60
moves/s with `DRM_STATS` on.

### 9. Doom hangs in `malloc(16 MB)` on aarch64

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

### 10. AF_INET loopback `bind()` → EINVAL

Found by the tokio spike: TCP loopback bind fails, so the tokio TCP subtest is skipped
while UDS passes. Low priority — Wayland and D-Bus need only UDS — but it is a real gap
in the smoltcp integration.

### 11. Deferred work and known limitations

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

- Commit item 1's fix (`targets/aarch64-unknown-kernel.json`, `sched/src/context.rs`),
  then delete that item. The temporary `hashprobe` staging hunk in
  `scripts/mkfs-f2fs-populated.py` has already been reverted — the working tree should
  show only those two files plus `README.md`.
- Untracked disk-image backups at the repo root
  (`f2fs-data0-aarch64.img.12h15-orig`, `.full-rebuild`, `.m7z2-orig-backup`,
  `f2fs-data0-x86_64.img.m7z2bak`) and `ports/busd/.work/` — delete or gitignore.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at
  exit 144.
