# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`26eebf0`); item 5 and the item 11
Mesa-modifier bullet updated the same day with a source-analysis wave over smithay
`efeb597` and the kernel DRM property/blob path. Same day, a second wave: item 2
(memfd tmpfs-slot leak) got a completed source-analysis pass and a prepared-but-unbuilt
patch, and a new item 3 was split out for the TGID defect the audit found along the way.

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
and fresh images, both arches, as of `26eebf0`: vfstest 36/0, drmsmoke 22/0, scmtest
26/0 (the 26th subtest, `inet_loopback_tcp`, added in `26eebf0`), wakepolltest 10/0,
forktest 3/0, epolltest 9/0 (the 9th subtest, `nested_epoll`, added in `4085b7f`),
polltest 6/0, sigtest 6/0, timertest 6/0 (the 6th subtest, `clock_monotonic_subtick`,
added in `75b32e3`), memtest 4/0, waittest 5/0 — all on x86_64. On aarch64, waittest
also came out 5/0 in this run rather than the previously recorded 3/2; the
`wait_on_process_group` flake simply did not fire this time, so treat either result as
acceptable.

**vmnet gotcha.** On a Mac with `socket_vmnet` installed, `driver.py` uses vmnet rather
than slirp, so the guest gets a `192.168.105.x` lease and `10.0.2.x` does not exist —
pings to `10.0.2.2` will silently see nothing. Force `-netdev user` to reproduce the
documented slirp configuration. Also, proven by an A/B control against a pre-patch
kernel: on slirp, aarch64 never prints the `[NET] DHCP configured` line, though it does
reach `10.0.2.2` from its statically configured `10.0.2.15`; x86_64 does print it.

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
  **Trap for next time** (`26eebf0`, `handle_send`): `read_user_buf` alone does not
  fault a lazy page in — it resolves through `virt_to_phys`, which returns `None`
  instead of faulting, and `sys_sendto` never calls `prefault_user`, only
  `validate_user_buf`. Swapping in `read_user_buf` on its own would have been a
  regression (first-touch send buffers would EFAULT). Either pair it with
  `prefault_user` (private to the syscall crate) or hoist the copy above the lock so
  the fault happens with nothing held — the fix actually used, matching the idiom the
  `IcmpUnbound` arm already uses.
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

**`RUST_LOG=trace` cannot read smithay's own damage-tracking decisions.**
`cosmic-comp/Cargo.toml:61-62` sets `release_max_level_info` on `tracing`, so `trace!`
calls are compiled out of the release build and the feature ceiling cannot be raised
additively. Kernel-side counters are the only instrument.

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
| 1 | Venus/virgl — working on both arches; vkcube is the next milestone | Feature | — |
| 2 | memfd burns a tmpfs slot per call — fix prepared, needs an in-flight refcount | Bug — latent DoS | — |
| 3 | `tmpfile_owner_of` does not canonicalise pid to TGID | Bug — kernel | — |
| 4 | `wl_display error 0 "Unknown id: 636"` | Bug | re-measure post-fix |
| 5 | Primary-plane recomposite (FB_DAMAGE_CLIPS is the instrument, not the fix) | Perf | — |
| 6 | evdev monotonic timestamps — recorded cause refuted, ready to re-land | Bug | — |
| 7 | Doom hangs in `malloc(16 MB)` on aarch64 | Bug | re-verify first |
| 8 | `listen()` twice returns EINVAL, deviating from Linux | Bug | — |
| 9 | `unused variable: port` warning in `handle_close` | Cleanup | — |
| 10 | Dead `init_main` / unreachable POSIX smoke tests | Cleanup | — |
| 11 | Deferred / known limitations | Mixed | — |

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

### 2. memfd burns a tmpfs slot per call — fix prepared, needs an in-flight refcount

**How it actually works, measured.** `MAX_TMP_FILES = 128` (`servers/vfs/src/lib.rs:264`)
bounds one flat `TMP_FILES` array (`:326`) shared by `/tmp`, `/dev/shm` **and**
`/run/user`, with a parallel `TMP_VMOS` (`:378`); allocation is a linear free-slot scan
and exhaustion returns **ENOSPC (-28)** (`:2808`, `:2819`). The reclamation machinery is
complete and correct — `tmp_drop_name` (`:2125`) frees the slot on unlink or marks it
`ephemeral` if an open fd still names it, and `tmp_release_ephemeral` (`:2341`) collects
the slot and its VMO frames on last close, called from `handle_close`, `release_vnode`
and the exit sweep. It is simply never triggered for memfd: because `sys_memfd_create`
never unlinks, `ephemeral` is never set, so a memfd slot is **never** reclaimed by any
path and closing the last fd frees nothing. One leak, at the name.

**The in-code comment's mechanism is refuted; its conclusion is right for a reason it
never named.** Every fd-side memfd operation destructures `VnodeKind::TmpFile { idx }` —
`handle_ftruncate` (`:4950`), the K1 mmap path via `tmpfile_owner_of` (`:459`, from
`vmo_acquire_frames` `:597`), `mark_memfd` (`:526`), read/write/lseek (`:3076`/`:3208`/
`:3447`), `handle_fstat` (`:6393`), seals (`:4068`, `:4079`), and the `f*` xattr and
mode/owner calls. The only name-keyed site is `handle_open` (`:2792` via `tmp_find`),
which nothing uses to reopen a memfd, and `tmp_find` (`:2042`) skips `ephemeral` anyway
— exactly the Linux invisibility we want. `scmtest::test_teardown_loop` already runs 150
rounds of create → ftruncate → mmap → unlink-then-close and passes.

**But unlink-alone would be an immediate hard regression.** `export_fd` (`:3630`)
deliberately takes no reference for a `TmpFile` ("lifetime is table-scan driven"), so an
SCM_RIGHTS fd between `sendmsg` and the peer's `recvmsg` is in **no** fd table. Today
the name pins the inode; once the node is nameless, `close(fd)` immediately after
`wl_shm_create_pool` — the standard libwayland/Mesa swrast idiom — frees the slot and
its VMO before `import_fd` installs it, and the next `memfd_create` recycles that idx
under the compositor. So the fix needs an in-flight refcount alongside the unlink.

**The `smithay-clipboard` OutOfMemory lead is NOT this bug** — and chasing it found a
separate real defect. Three disqualifiers: the panic carries **ENOMEM (12)** while slot
exhaustion returns **ENOSPC (28)**; it fires at t≈8.5 s with only ~6 memfds ever
created; and `MultiPool::new` runs once per process, so nothing accumulates. The actual
cause is that **`tmpfile_owner_of` (`servers/vfs/src/lib.rs:459-461`) does not
canonicalise `pid` to the TGID**, while `vfs_get_node_kind` (`:810`) does. On a spawned
thread the kind resolves as `TmpFile`, `sys_mmap` takes the K1 branch,
`vmo_acquire_frames(pid=TID)` → `find_tbl(TID)` → `None`, and
`kernel/src/syscall.rs:1699` returns ENOMEM. The panicking thread is literally named
`smithay-clipboard`. `mark_memfd` (`syscall.rs:7115`) silently no-ops for the same
reason. This is the identical lesson `36f62d0` already learned for
`install_dmabuf_vmo`/`dmabuf_handle_of`, which pass `sched::tgid_of(pid)` explicitly.
**The TGID fix is required for this item anyway** — without a VMO the memfd falls back
to the 32 KiB inline data path.

**Prepared patch** (397 lines, **unbuilt**, verified to `git apply --check` cleanly at
`aa2329c`) at `~/code/leandros-artifacts/notes/m9-memfd-tgid/memfd_unlink.patch`,
touching `kernel/src/syscall.rs`, `servers/vfs/src/lib.rs` and
`userland/scmtest/src/main.rs`. It canonicalises `tmpfile_owner_of` to the TGID; adds
`TMP_INFLIGHT` with inc/dec/mask helpers so `tmp_open_fd_mask` and
`tmp_release_ephemeral` both honour in-flight fds (lock nesting stays FD_TABLES →
TMP_INFLIGHT → TMP_FILES); takes an in-flight reference in `export_fd` and drops it in
`import_fd` after the table install and on both EMFILE paths, and in `drop_transfer`;
and replaces the KNOWN-LEAK comment in `sys_memfd_create` with a `VFS_UNLINK`, matching
`36f62d0` in shape.

**Verification.** `scmtest` **26/0 → 28/0**. `memfd_anonymous_reclaim` is the pass
criterion for the item: `stat("/tmp/memfd:anonprobe")` must fail while the fd is open,
then 300 rounds (>2×128) of create → ftruncate → MAP_SHARED mmap → write/verify →
munmap → close with **no manual unlink**, passing 300/300; today it dies with ENOSPC
around round 120, and `test_teardown_loop` cannot catch it because it unlinks by hand.
`memfd_inflight_close` guards the new hazard: the parent stamps a pattern, munmaps,
`sendmsg`s the fd, closes it, and only then releases the child, which must `recvmsg`,
mmap and see the pattern — this fails deterministically without the `TMP_INFLIGHT`
half. No other baseline moves (`test_teardown_loop`'s `unlink` calls will return ENOENT
but the test discards the return). For the TGID half the on-target criterion is the
live COSMIC session: `thread 'smithay-clipboard' panicked … Create(Os { code: 12 })`
must be **absent** from the serial log — it is present in every one of ~10 archived
runs, so its disappearance is a clean signal.

### 3. `tmpfile_owner_of` does not canonicalise pid to TGID

`tmpfile_owner_of` (`servers/vfs/src/lib.rs:459-461`) keys on the raw pid while
`vfs_get_node_kind` (`:810`) canonicalises to the TGID, so any memfd `mmap` from a
spawned **thread** (as opposed to the process's main thread) returns ENOMEM
(`kernel/src/syscall.rs:1699`) instead of resolving the shared VMO, and `mark_memfd`
(`syscall.rs:7115`) silently no-ops for the same reason. Observed as the
`smithay-clipboard` thread panicking with `Failed to create memory pool … OutOfMemory`
(`Os { code: 12 }`) in `cosmic-files-applet` during COSMIC sessions — pre-existing,
session survives it, but it is a real correctness bug, not a benign symptom. `36f62d0`
already fixed the same class of bug for the dmabuf path, by passing
`sched::tgid_of(pid)` explicitly at the two dmabuf-side call sites. The fix for this
item is included in the item 2 patch above, but it is an independent defect worth its
own line — it breaks threaded memfd users generally, not just this one.

### 4. `wl_display error 0 "Unknown id: 636"` — panel↔comp desync

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

### 5. Primary-plane recomposite (FB_DAMAGE_CLIPS is the instrument, not the fix)

**What we already have, measured.** The property is fully plumbed, not merely
advertised: `PROP_FB_DAMAGE_CLIPS = 51` as `PropKind::Blob` in `PROPS`
(`drivers/src/drm_device_interface.rs:164`) and `PLANE_COMMON` (`:219`), correctly
omitted from `CURSOR_PLANE`; `CREATEPROPBLOB` (`:2258`), `DESTROYPROPBLOB` (`:2275`) and
`GETPROPBLOB` (`:2286`) are all implemented over a `BLOBS` map (`:1148`); and the atomic
path already reads the value into `AtomicPlaneReq::damage_blob` (`:2414`). We simply
never act on it — the present path calls `handle_flip_page` unconditionally, doing a
full-surface scale plus a full-screen `gpu.flush`.

**The item's premise was not established by its own evidence.** `flips/s == atomic/s ==
cursor_mv/s` is a **tautology of our kernel's counter**, not an observation about
smithay. smithay keeps a skipped plane in the request (`compositor/mod.rs:804`,
`!state.skip || state.config.is_some()`) and the skip branch clones the previous frame's
config verbatim, so `FB_ID` is re-sent either way; our handler counts a flip for any
commit naming a nonzero primary `FB_ID`. The counter reads identically whether smithay
skipped or repainted.

**Kernel-side `FB_DAMAGE_CLIPS` cannot make smithay skip.** The decision is made
entirely inside `OutputDamageTracker` at `compositor/mod.rs:2306-2320`, *before* the
property is written to the kernel at `surface/atomic.rs:1278-1284`, and there is no
feedback path from the driver back into the damage tracker (smithay pin `efeb597`, per
`cosmic-comp/Cargo.lock:4816`). Two other candidate causes are also ruled out from
source: a missing plane capability or fallback path is excluded because `cursor_mv =
6.0/s` with one total cursor upload proves `try_assign_cursor_plane` succeeded, which
already requires ATOMIC, universal planes, size caps, gbm and a passing `TEST_ONLY`; and
cursor-overlaps-primary is excluded because a cursor element assigned to the cursor
plane is never pushed into `primary_plane_elements`.

To reach the skip, all of: the primary buffer is a swapchain slot; no direct scanout
last frame; and `render_output` returned `skipped()`, which needs both no element
instance/commit/z-order change **and** `age > 0 && last_state.old_damage.len() >= age` —
otherwise smithay clears the damage and pushes the whole output geometry
(`renderer/damage/mod.rs:741-759`). **Inferred, well-supported:** we fail that third
condition. 6.0 frames/s at 1280x800 on softpipe is ~160 ms/frame, the cost signature of
a real full-screen recomposite; a skipped primary costs nothing and the loop would run
near the flip-delivery ceiling.

**Why the work is still worth doing, for a different reason than this item used to
state.** The blob smithay hands us *is* the damage tracker's output, so decoding it
turns an unanswerable client-side question into a kernel-side measurement with no COSMIC
rebuild. And there is a real kernel defect underneath: **when smithay does skip the
primary, we currently do a full-screen scale plus full-screen `TRANSFER_TO_HOST` and
`RESOURCE_FLUSH` anyway** — which would cancel the win even once the client side is
fixed. Direct perf value of the property alone is small (~1.7 ms/flip x 6 flips/s, about
1% CPU).

**A prepared patch** (357 lines, **unbuilt**, `drivers/` only) is at
`~/code/leandros-artifacts/notes/m9-fb-damage-clips/fb_damage_clips.patch`, verified to
`git apply --check` cleanly at `a9621b0`. It adds `DrmDevice::present_damaged` (clamped
rects mapped with the same nearest-neighbour arithmetic `perform_software_scaling` uses,
one flush over the bounding union), `DAMAGE_{FULL,RECT,SKIP,PX}` and `BLOBS_CREATED`
counters on the `DRMSTAT` line, a `damage_rects` blob decoder that returns `None` (=
assume full damage) for any unusable blob rather than erroring — rejecting a commit over
a hint would stall the compositor — and a Skip/Rects/Full dispatch where Skip fires only
on smithay's verbatim-config replay. **Two behaviour changes to know about:**
`FLIPS_SUBMITTED` will count presents that moved pixels rather than atomic commits, so
any harness asserting `flips == atomic` will now "fail" by design; and `present_damaged`
updates only `plane.fb_id`, relying on a preceding full present for geometry, which is
guaranteed since modesets always take the Full path.

**Verification is diagnostic-first.** Gate on aarch64 (HVF, the recorded 6.0 baseline is
aarch64 at 1280x800), 60 pointer moves/s, >=60 s of motion, `DRM_STATS` on. Sanity check:
`dmg_full + dmg_rect + dmg_skip` must equal `atomic`. Then read `dmg_px / dmg_rect`
against 1280x800 = 1,024,000 px (`0xFA000`, counters print in hex): near-full means the
compositor damages the whole output every frame and **the blocker is client-side — stop,
no further kernel work moves flips/s**; under ~5% means damage tracking works and the
perf pass criterion is `flips/s <= 2.0` while `cursor_mv/s >= 6.0`. Three controls are
mandatory: an `evpush` guest-side counter climbing at ~60/s (QMP accepting a move does
not prove it reached the guest ring), `cursor_mv/s` must not fall relative to pre-patch
(`flips/s -> 0` with `cursor_mv/s -> 0` is a dead pointer, not a win — revert on that
signature), and a stale-pixel check, since damage-bounded present makes a tracking error
show up as stale pixels rather than a crash: let the panel clock run >=60 s, take two
screendumps >=2 s apart and confirm the digits differ, then force one full present and
confirm it is pixel-identical. Note the cursor will not appear in `screendump` now that
it is on the hardware plane. Plus `drmsmoke` 22/0 both arches and `idletest`.

### 6. evdev monotonic timestamps — recorded cause refuted, ready to re-land

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

### 7. Doom hangs in `malloc(16 MB)` on aarch64

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

### 8. `listen()` twice returns EINVAL, deviating from Linux

`handle_listen` (`servers/net/src/lib.rs`) matches only `SockState::InetBound`, so a
second `listen()` on an already-listening socket falls through to `_ => err_reply(-22)`.
On Linux a second `listen()` succeeds and simply updates the backlog. Confirmed by
measurement (`second_errno=22`). Low priority, but a real POSIX deviation that a server
framework could trip on. Discovered during AF_INET loopback verification (`26eebf0`).

### 9. `unused variable: port` warning in `handle_close`

`servers/net/src/lib.rs:2423`, in `handle_close`: the `InetListening`/`InetBound`
rework in `26eebf0` stopped using `bound_port` there. Cosmetic; a one-character `_port`
fixes it. Left as-is deliberately so that patch could land and be reviewed verbatim.

### 10. Dead `init_main` / unreachable POSIX smoke tests

`init_server::init_main()` (`servers/init/src/lib.rs:2651`) is referenced nowhere in
the kernel; the only mention outside its own file is a stale doc comment at
`kernel/src/init.rs:4`. Everything it calls — including `run_posix_tests()` and the
`t_af_inet_loopback` self-test — is unreachable, and "POSIX smoke tests" appears in no
serial log. Either wire it back into the boot path or delete it, but do not leave a
self-test that reads as coverage and provides none. **Discovered because the AF_INET
loopback work (`26eebf0`) cited that self-test as evidence** — a dead test is worse
than no test, because it gets cited.

### 11. Deferred work and known limitations

- **Mesa modifier support — needs re-verification.** The claim that our GBM lacking
  `gbm_bo_create_with_modifiers2` means smithay cannot build a reusing swapchain and
  reallocates per frame was **not confirmed against smithay's source**, and may be
  wrong: at the pinned revision, `allocator/swapchain.rs:158-178` caches slots and only
  allocates when `buffer.is_none()`, and `allocator/gbm.rs:204-219` has a documented
  fallback for Invalid/Linear modifiers in `create_buffer_object`. The per-frame-
  reallocation conclusion is unverified; the 128-dmabuf-fd burn in ~1 s and the
  `MAX_FDS` 64→128 raise are separately observed facts and still stand. Revisit with
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
