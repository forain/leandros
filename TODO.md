# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`75b32e3`).

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment *unmodified* (source: `../cosmic-epoch`)
on both x86_64 and aarch64 under QEMU. No COSMIC source patches; build-configuration
flags (`--no-default-features`) are allowed. Everything beneath COSMIC — kernel, libc,
system libraries, daemons — is ours.

**Where it stands.** The desktop runs on both arches: cosmic-session → cosmic-comp on
KMS/softpipe → busd → cosmic-bg + cosmic-panel renders a wallpaper plus a full-width
panel bar with an embedded Wayland client. The panel clock ticks on both arches
(`4085b7f`: `poll_fd_state` had no epoll-fd case, so a nested epoll fd fell into the
socket branch and reported `POLLNVAL` as readiness on every pass, starving the frame
callback that reopens cosmic-panel's `has_frame` gate). Remaining work is quality and
performance, not bring-up. The full suite is green on freshly-built release binaries
and fresh images, both arches, as of `75b32e3`: vfstest 36/0, drmsmoke 22/0, scmtest
25/0, wakepolltest 10/0, forktest 3/0, epolltest 9/0 (the 9th subtest, `nested_epoll`,
added in `4085b7f`), polltest 6/0, sigtest 6/0, timertest 6/0 (the 6th subtest,
`clock_monotonic_subtick`, added in `75b32e3`), memtest 4/0, waittest 5/0 — all on
x86_64. On aarch64, waittest also came out 5/0 in that run rather than the previously
recorded 3/2; the `wait_on_process_group` flake simply did not fire this time, so
treat either result as acceptable. Landing the AF_INET loopback patch (item 7) will
move scmtest 25 → 26.

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
- Regression images must be freshly regenerated — run vfstest **exactly once** per
  freshly generated image. A dirty f2fs image produces phantom failures: an A/B
  control (identical signature with and without an unrelated kernel change) showed a
  second vfstest run on the same image fails three subtests —
  `chroot_confines_symlink_resolution`, `xattr_list_tmpfs` and `xattr_list_f2fs`.

**Diagnostics in-tree, all compiled out by default** — flip to `true`, measure,
flip back before committing:

| Flag | File | Measures |
|---|---|---|
| `DRM_STATS` | `drivers/src/drm_device_interface.rs:1230` | flips, cursor up/mv, atomic, atest, cplane |
| `CURSOR_DEBUG` | `drivers/src/virtio_gpu.rs:342` | cursor queue setup + selftest |
| `mm::gap2::ON` | `mm/src/gap2.rs:17` | memfd/MAP_SHARED path + frame checksum sampler |

**Evidence lives outside this repo.** Run logs, screenshots, research notes and test
harnesses are in `~/code/leandros-artifacts/notes/`. Design docs that are still
execution-ready are in `docs/design/`. `notes/m9-af-inet-loopback/` holds the
verified, ready-to-land patch for item 7.

**Explicitly out of scope** (all degrade gracefully or are non-fatal): XWayland,
PipeWire/audio for COSMIC, NetworkManager, UPower, accountsservice, greetd +
cosmic-greeter, cosmic-workspaces' wgpu path, hotplug, VT switching, multi-seat.

---

## Open work

| # | Item | Category | Blocked on |
|---|---|---|---|
| 1 | Venus/virgl — working on both arches; vkcube is the next milestone | Feature | — |
| 2 | memfd burns a tmpfs slot per call | Bug — latent DoS | — |
| 3 | `wl_display error 0 "Unknown id: 636"` | Bug | re-measure post-fix |
| 4 | `FB_DAMAGE_CLIPS` / primary-plane recomposite | Perf | — |
| 5 | evdev monotonic timestamps — recorded cause refuted, ready to re-land | Bug | — |
| 6 | Doom hangs in `malloc(16 MB)` on aarch64 | Bug | re-verify first |
| 7 | AF_INET loopback — patch verified on both arches, ready to land | Bug | — |
| 8 | `handle_send` copies user memory under the stack lock | Bug — kernel | — |
| 9 | Dead `init_main` / unreachable POSIX smoke tests | Cleanup | — |
| 10 | Deferred / known limitations | Mixed | — |

---

### 1. Venus/virgl — working on both arches; vkcube is the next milestone

The round-trip works and the TCG hang is fixed. On the Linux box
(`forain@172.16.158.150`, EndeavourOS, virglrenderer 1.3.0, QEMU 11.0.1 — already
installed, nothing to add; it is **Arch, not Debian**, so the old `apt install` line
was wrong), on softfloat HEAD with fresh images: `venustest` is **68/68** and `vktest`
is **0 failures** on x86_64/KVM, x86_64/TCG **and** aarch64/TCG — the first-ever
aarch64 Vulkan pass, opening a real GPU through Mesa's Venus ICD (`Virtio-GPU Venus
(AMD Ryzen 9 7950X (RADV RAPHAEL_MENDOCINO))`, `vkCreateDevice` VK_SUCCESS).

The TCG hang at `vkEnumeratePhysicalDevices` was **not** a GPU bug: `CLOCK_MONOTONIC`
was advancing in 10 ms steps, which starved Mesa's Venus ring notify throttle. Fixed
in `75b32e3` ("time: give CLOCK_MONOTONIC sub-tick resolution").

Venus needs the device line `-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G
-display egl-headless`. `scripts/run-qemu.sh` does **not** pass these, and on x86_64 it
selects `virtio-vga` (no GL at all), so the in-tree harness cannot exercise Venus — only
bespoke wave scripts can. Worth fixing. Reminder: `-nographic` silently overrides
`-display`.

The old "`venustest` fails 29 / `host lacks VIRGL/BLOB/CONTEXT_INIT`" line was a
**macOS-host** artifact (no EGL) — not a code defect, and not the state on Linux.
macOS-has-no-EGL and rutabaga-is-a-dead-end both remain accurate.

`vkcube` is **not** yet a runnable follow-on: it has never been built for LeandrOS (no
binary or source in the repo or in `leandros-artifacts`;
`scripts/mkfs-f2fs-populated.py` stages only `vktest` + `libvulkan_virtio.so`), it links
the Khronos `libvulkan.so.1` loader that we deliberately do not ship (`vktest` exists
precisely to bypass the loader and `dlopen` the ICD directly), and no WSI has been
chosen among the ICD's `VK_KHR_wayland_surface` / `VK_KHR_display` /
`VK_EXT_headless_surface` / `VK_EXT_acquire_drm_display`. That is the M3 rendering
milestone.

### 2. memfd burns a tmpfs slot per call

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

A `smithay-clipboard` thread in `cosmic-files-applet` panics with `Failed to create
memory pool … OutOfMemory` during COSMIC sessions. It is pre-existing and the session
survives it, but it is exactly the symptom this item predicts, and is a candidate
reproducer for the tmpfs-slot exhaustion.

### 3. `wl_display error 0 "Unknown id: 636"` — panel↔comp desync

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

### 4. `FB_DAMAGE_CLIPS` / primary-plane recomposite

The cursor plane landed and moved pointer motion from **0.9 → 6.0 page flips/s**, with
the cursor image uploaded exactly once and zero pixel traffic per move. But the honest
caveat from that measurement is `flips/s == atomic/s == cursor_mv/s`: smithay still
flips the **primary** plane on every cursor frame. The end state
(`compositor/mod.rs:2318` "skipping primary plane, no damage") was not reached.

This is the remaining pointer-latency win, and it is on the primary plane, not the
cursor. `FB_DAMAGE_CLIPS` is already advertised in the plane property table.

### 5. evdev monotonic timestamps — recorded cause refuted, ready to re-land

The recorded cause — "libinput rejects the `cntvct`-derived timestamps" — is **wrong**,
refuted by reading libinput 1.27.1 (on disk, matching the shipped `libinput.so.10.13.0`).
Our pointer is the virtio-tablet, an **absolute** pointer (`servers/evdev/src/lib.rs:46-48`,
commit `e92f22b`), and `evdev-fallback.c:207-221` passes `time` straight through to
`pointer_notify_motion_absolute` without ever using it. There is no dt, no filter and no
acceleration on the absolute path — those are reached only from
`fallback_flush_relative_motion` (`:169-198`). **The zero-dt division hazard that
motivated the change does not apply to the device we actually have.** Every other
consumer of `input_event.time` was checked and is non-fatal: `evdev_note_time_delay`
(`evdev.c:1109-1133`) is a pure log that returns early when the event time is in the
future; the out-of-order-timestamp check (`libinput.c:2309-2320`) is inside `#if 0`; the
timer sanity checks (`timer.c:94-112`) are `#ifndef NDEBUG` and verifiably absent from
the shipped `.so`; a wrong epoch only mis-arms button/scroll/debounce timers, which
motion never passes through; and cosmic-comp's `PointerMotionAbsolute` handler
(`input/mod.rs:675-707`) gates nothing on the timestamp. No value of `input_event.time`
— wrong units, wrong epoch, coarse or duplicated — can suppress absolute-pointer motion
in this stack.

The likely real cause, **inferred but well-supported**: the aarch64 FP/SIMD clobber
fixed in `05f7279`, root-caused three days *after* those runs. The change inlined a
copy of `drivers::snd::monotonic_us()` into evdev, putting 128-bit arithmetic (`cnt as
u128 * 1e6 / frq as u128`, lowered to `__udivti3`) into **IRQ context** — `push_event`
is called from `arch/aarch64/src/timer.rs:80` and `exception.rs:72`, both inside the
interrupt — at a time when the kernel was built `+neon,+fp-armv8` with no vector state
in the EL0 trap frame. This explains the detail that "libinput is picky" never could:
the same `monotonic_us()` was already running in that build under `DRM_STATS` and was
harmless, because on the SVC path AAPCS64 permits a call to clobber v0-v7/v16-v31,
whereas an interrupt has no such licence and lands on the interrupted thread's live
vector state at an arbitrary instruction. The observed signature — total,
path-independent failure of a float-heavy compositor on the atomic path **and** on a
legacy control — is what ~120 vector corruptions/s looks like.

Also on the record: the original experiment was **confounded**. Run s4 changed two
variables at once (the evdev revert *and* `COSMIC_DISABLE_DIRECT_SCANOUT` →
`COSMIC_DISABLE_OVERLAY_SCANOUT`), so the evdev change was never isolated by a
single-variable A/B.

Verdict: **re-land it** — but note that what unblocks it is the softfloat kernel
(`05f7279`), which makes an IRQ-context vector clobber structurally impossible, **not**
the new interpolated clock. Resolution was never the cause. The new `monotonic_ns()` is
nonetheless the correct source to use, for three independent reasons: it shares the
tick counter's epoch and therefore `sys_clock_gettime`'s, which is the only thing
libinput's `EVIOCSCLOCKID(CLOCK_MONOTONIC)` contract actually requires; it is
non-decreasing by construction (`fetch_max`); and it has sub-tick resolution. The raw
`monotonic_us()` had none of the three — its epoch is counter-zero rather than
tick-zero, and on x86_64 it hardcodes a 1 GHz TSC. **Do not re-land using
`monotonic_us()`.**

Honest scope: the user-visible benefit today is modest — better `wl_pointer` stamps for
client-side timing, and fewer "event processing lagging behind" warnings from
`evdev.c:1128` (20 ms threshold, which gets closer once `clock_gettime` is finer while
evdev stays 10 ms-quantized and one tick behind). The large win, accelerated dt, only
materialises if a **relative** pointer is ever attached.

A prepared patch (138 lines, **unbuilt**) is at
`~/code/leandros-artifacts/notes/m9-evdev-timestamps/evdev_timestamps.patch`; it sits on
top of the two m9 clock patches. It exports `arch_monotonic_ns()` from both arch
crates, declares it in evdev's existing `extern "C"` block (evdev cannot reach `drivers`
or `arch` directly — a dependency cycle, which is why the original inlined a copy), and
stamps `push_event` from it, reading the counter **before** `arch_interrupt_save()` and
before `DEVICES.lock()` so no lock is held and no user memory is touched.

Verification, given this item's history of a change that looked fine and silently
killed input: pre-flight with `userland/evtest2` on aarch64, which already reports
`motion_ts_monotonic` — pass requires that subtest green **and** `tv_usec` values not
all multiples of 10000, which proves units, monotonicity and resolution for almost no
cost. Main gate on aarch64 with `DRM_STATS` on
(`drivers/src/drm_device_interface.rs:1230`) at 60 moves/s: pass reproduces the s4
numbers (≈6.0 flips/s, ≈6.0 cursor mv/s, 0.00 cursor uploads/s), fail is the reverted
signature (`flips_sub` frozen, `curs_mv=0` across 1000+ delivered moves). Run the
legacy-path control on a build differing **only** by this patch, and add a guest-side
event counter — every previous run only proved QMP accepted the moves, never that they
reached the guest ring.

### 6. Doom hangs in `malloc(16 MB)` on aarch64

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

### 7. AF_INET loopback — patch verified on both arches, ready to land

The patch is built and verified. It **compiled clean on the first release build** — no
type or borrow errors, including the `stack_for` guard in `accept_on`/`listen_on` its
author expected to break. One correction was needed: `write_sockaddr_in` used aligned
`core::ptr::read`/`write` on the user-supplied `addrlen` pointer, which is UB if the
caller passes an odd address; it now uses `read_unaligned`/`write_unaligned`, matching
the idiom the rest of the file already uses for every user pointer. That two-line
change is the entire delta from the original patch.

The verified patch is at
`~/code/leandros-artifacts/notes/m9-af-inet-loopback/af_inet_loopback_verified.patch`
(1065 lines, `git apply --check` clean against `d4e3746`). Verified on the Linux box,
both arches, fresh f2fs images, vfstest first, counts parsed from each test's own
`--- <name> done ---` markers rather than the harness's line-attribution counters:

- `[NET] Loopback interface 127.0.0.1/8 up` on both arches.
- `inet_loopback_tcp: PASS` on both, with `[inet] getsockname port=34709 addr_ok=1` and
  `[inet] c2s=1 s2c=1`. **`scmtest` moves 25 → 26 subtests** — update the baseline when
  this lands.
- Everything else identical pre- and post-patch on both arches; the single aarch64
  `waittest` failure is the documented `wait_on_process_group` flake (re-run five
  times: 4 PASS, 1 FAIL).
- Two extra branch-coverage subtests were added temporarily and then reverted:
  `inet_inaddr_any` passed (INADDR_ANY arms a listener on **both** stacks; `accept`
  tries the NIC stack first, finds nothing, falls through to loopback) and
  `inet_double_listen` passed (`second_errno=22`).
- **Unplanned but valuable**: the x86_64 harness passes no `-netdev`, and q35's default
  NIC is e1000 which our driver ignores — so x86_64 ran with **no NIC at all**,
  `NET_STACK` was `None`, and `inet_loopback_tcp` still passed. That is the hardest
  case the patch was designed for, covered for free.

Two corrections to what this item previously said. First, **the boot-time self-test is
dead code, not failing**: `t_af_inet_loopback` lives in `run_posix_tests()`, called
only from `init_server::init_main()` (`servers/init/src/lib.rs:2651`), and `init_main`
is referenced nowhere in the kernel — the only hit outside its own file is a stale doc
comment at `kernel/src/init.rs:4`. The real boot path loads the userspace init ELF from
initrd, and the string "POSIX smoke tests" appears **zero** times in any serial log
including pre-patch baselines. The patch's `servers/init` changes are therefore
unreachable code. Second, the residual `ping 127.0.0.1` behaviour is **worse than
described**: on a NIC-equipped guest it returns 4/4 replies, because slirp forwards the
echo to the *host's* loopback and the host answers — so it looks like guest loopback
ICMP works when it does not. Proven on x86_64, which has no NIC: there the same command
gives `ping: sendto failed` ×4 while `inet_loopback_tcp` passes in the same boot.
Off-box networking is intact (aarch64 `[NET] DHCP configured, address: 10.0.2.15`,
`ping 10.0.2.2` 4/4).

Remaining known gap: `getsockname` on an AF_INET socket that has never bound still
returns `sa_family = 0, addrlen = 2`, because a fresh socket is `SockState::Unbound`
which `inet_local_endpoint` does not match. Not on the tokio path.

### 8. `handle_send` copies user memory under the stack lock

`handle_send`'s `InetConnected` arm calls `core::ptr::copy_nonoverlapping` from user
memory **while holding the stack spinlock**, violating the standing invariant that the
kernel must never touch user memory under a server lock or any IRQ-off spinlock (the
same hazard shape that once froze all four vCPUs, fixed in `82d0cc3`). This is
**pre-existing**, not introduced by the loopback work — but the loopback patch
rewrites that exact line (`NET_STACK.lock()` → `stack_for(lo)`) without hoisting the
copy out, so it survives into the new code and should be fixed in the same area.
Everything the loopback patch *adds* respects the invariant correctly:
`write_sockaddr_in` is called only after every guard is dropped at all three call
sites, which is itself an improvement over the old `handle_accept`, which wrote the
peer `sockaddr` while holding `NET_STACK`. The fix is to read the user buffer into a
kernel-side buffer before acquiring the lock, using `read_user_buf`.

### 9. Dead `init_main` / unreachable POSIX smoke tests

`init_server::init_main()` (`servers/init/src/lib.rs:2651`) is referenced nowhere in
the kernel; the only mention outside its own file is a stale doc comment at
`kernel/src/init.rs:4`. Everything it calls — including `run_posix_tests()` and the
`t_af_inet_loopback` self-test — is unreachable, and "POSIX smoke tests" appears in no
serial log. Either wire it back into the boot path or delete it, but do not leave a
self-test that reads as coverage and provides none. **Discovered because TODO item 7
cited that self-test as evidence** — a dead test is worse than no test, because it gets
cited.

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
- **DRM page-flip event timestamps** (`drivers/src/drm_device_interface.rs:394,398-400`)
  are still built from the 100 Hz tick scheme, and smithay reads them for presentation
  feedback — worth moving to the interpolated clock in the same sweep.

---

## Housekeeping

- Untracked disk-image backups at the repo root
  (`f2fs-data0-aarch64.img.12h15-orig`, `.full-rebuild`, `.m7z2-orig-backup`,
  `f2fs-data0-x86_64.img.m7z2bak`) and `ports/busd/.work/` are now gitignored
  (`f2fs-data0-*.img.*`, `ports/*/.work/`); delete them by hand when no longer needed.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at
  exit 144.
