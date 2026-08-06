# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`05bb0fe`); item 4 and the item 10
Mesa-modifier bullet updated the same day with a source-analysis wave over smithay
`efeb597` and the kernel DRM property/blob path. Same day, a second wave: item 2
(memfd tmpfs-slot leak) got a completed source-analysis pass and a prepared-but-unbuilt
patch, and a new item 3 was split out for the TGID defect the audit found along the way.
Same day, a third wave retired the former item 7 (Doom hang): measured on the Linux box
at `295136c` with fresh images, Doom runs on both arches, including the literal
`-mb 16` case — see the softfloat note in Standing context and the allocator note in
item 10 for what it leaves behind. Same day, a fourth wave covers this reconciliation:
items 5-7 (`listen()` twice, the `handle_close` warning, and the dead `init-server`
crate) got a combined, verified-applicable patch at
`~/code/leandros-artifacts/notes/m9-small-fixes/small_fixes.patch`, confirmed to stack
cleanly in both orders with the item 2 and item 4 patches; two new items, 8 and 9,
were split out for AF_UNIX `listen()` laxness and the missing TIME_WAIT state found
along the way. Same day, a fifth wave closed two more: the former item 4
(`wl_display error 0 "Unknown id: 636"`) does not reproduce — a fresh aarch64/HVF
release build and fresh images ran a 200 s COSMIC session with a 30 s pointer-motion
window and hit zero `Unknown id`, `Broken pipe`, `PANEL MAIN ERR` or `wl_display#1:
error` occurrences, confirming the item's own hypothesis (an FP/SIMD-clobber artifact
— see the tally in Standing context) and clearing the AF_UNIX SCM_RIGHTS path it had
implicated (`scmtest` 26/0 both arches); and the former item 6 (evdev monotonic
timestamps) landed as `05bb0fe`, verified by `evtest2` 8/0 (`motion_ts_monotonic`,
`motion_ts_subtick`) and the cursor gate (8.50 flips/s, 8.50 cursor mv/s, 0.00 cursor
uploads/s) against a legacy-path control confirmed genuinely legacy (`atomic=0
atest=0 cplane=0`). `05bb0fe` also added the `evpush` guest-side evdev-event counter
to the `[DRMSTAT]` diagnostics (see the table below). Same day, a sixth wave closed out
a design pass on item 1 (Venus/virgl): the 68/68 `venustest` and 0-failure `vktest`
results stand, but `vktest` stops at `vkCreateDevice` — no GPU work has ever actually
been submitted from LeandrOS — so item 1 is rewritten around a full M3 design at
`~/code/leandros-artifacts/notes/m9-m3-vulkan/m3-vulkan-design.md`, scoping M3 as an
offscreen `vkrender` with CPU readback rather than `vkcube`, and preparing (not
building) a `run-qemu.sh --venus` patch. Two new items were split out of the design's
kernel-gap findings: item 2 (`PRIME_HANDLE_TO_FD` refuses Venus blob handles, paired
with the `SIMULATE_SYNCOBJ` zero-size-execbuffer gap) and item 3 (`driver.py` has no GL
path, so the `run-leandros` skill cannot reach Venus at all). Former items 2-9 shifted
down to 4-11 and the former item 10 (deferred work) is now item 12.

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
acceptable. This wave (`05bb0fe`) also ran `idletest` 2/0 and `evtest2` 8/0 green on
both arches; neither was previously listed in this baseline.

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
  **Inferred, not bisected:** this is also most likely what retired the former item 7
  (Doom hang in `malloc(16 MB)` on aarch64, deleted 2026-08-06 — it now runs both
  arches on fresh images). Doom is compiled `clang --target=aarch64-unknown-none -O2`,
  a **hardfloat** target, so clang freely lowers inlined `memcpy`/`memset`/struct
  copies through `q` registers; `Z_Init`/`W_Init` is the phase of maximum cold-page
  exposure in a 1.45 MB static binary, and a demand-paging fault there under the old
  `+neon` kernel clobbering a `q` register holding a loop bound or pointer is exactly
  the "hangs with no output, no fault" shape that was observed. `75b32e3` (sub-tick
  `CLOCK_MONOTONIC`) is a weaker secondary candidate. Also retired by it, not by work
  aimed at it: the former item 4 (`wl_display error 0 "Unknown id: 636"`, deleted
  2026-08-06 — a 200 s COSMIC session with a 30 s pointer-motion window showed zero
  recurrences on the fixed kernel). The suspicion recorded against that item's AF_UNIX
  `SCM_RIGHTS` path was never borne out — `scmtest` covers that path at 26/0 on both
  arches. Doom is the fifth and id 636 the sixth thing this session traced to this
  clobber, directly or as the retiring cause.
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
| `evpush` (within `DRM_STATS`) | `drivers/src/drm_device_interface.rs` (`[DRMSTAT]` line) + `servers/evdev::events_pushed()` | guest-side evdev events pushed — distinguishes "the compositor ignored the moves" from "the moves never reached the guest ring" |

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
| 1 | Venus works; M3 is a render + readback test, not vkcube | Feature | — |
| 2 | `PRIME_HANDLE_TO_FD` rejects Venus blob handles | Bug | — |
| 3 | `driver.py` has no GL path at all | Bug | confirming screendump under egl-headless |
| 4 | memfd burns a tmpfs slot per call — fix prepared, needs an in-flight refcount | Bug — latent DoS | — |
| 5 | `tmpfile_owner_of` does not canonicalise pid to TGID | Bug — kernel | — |
| 6 | Primary-plane recomposite (FB_DAMAGE_CLIPS is the instrument, not the fix) | Perf | — |
| 7 | `listen()` twice returns EINVAL — fix prepared | Bug | — |
| 8 | `unused variable: port` in `handle_close` — not a leak, fix prepared | Cleanup | — |
| 9 | Delete the unreachable `init-server` crate | Cleanup | — |
| 10 | AF_UNIX `listen()` is lax in the opposite direction | Bug | — |
| 11 | No TIME_WAIT — ports are instantly reusable | Bug | — |
| 12 | Deferred / known limitations | Mixed | — |

---

### 1. Venus works; M3 is a render + readback test, not vkcube

**Measured**, on the Linux box (`forain@172.16.158.150`, EndeavourOS, virglrenderer
1.3.0, QEMU 11.0.1 — already installed, nothing to add; it is **Arch, not Debian**, so
the old `apt install` line was wrong), on softfloat HEAD with fresh images: `venustest`
is **68/68** and `vktest` is **0 failures** on x86_64/KVM, x86_64/TCG **and**
aarch64/TCG, opening a real GPU through Mesa's Venus ICD (`Virtio-GPU Venus (AMD Ryzen 9
7950X (RADV RAPHAEL_MENDOCINO))`, `vkCreateDevice` VK_SUCCESS). The old "`venustest`
fails 29 / `host lacks VIRGL/BLOB/CONTEXT_INIT`" line was a **macOS-host** artifact (no
EGL) — not a code defect, and not the state on Linux.

**The decisive finding: no GPU work has ever been submitted from LeandrOS.** `vktest`
stops at `vkCreateDevice` — everything proven so far is device *discovery*, not
rendering. A full design is at
`~/code/leandros-artifacts/notes/m9-m3-vulkan/m3-vulkan-design.md` (582 lines).

**M3 is `vkrender`, not `vkcube`:** an offscreen render with CPU readback and pixel
assertions, then a blit to a DRM dumb buffer for scanout. Subtests escalate —
shaderless `vkCmdFillBuffer`, then compute, then a triangle into an offscreen colour
attachment copied back through `HOST_VISIBLE|HOST_COHERENT` memory. It needs **no WSI
and no loader**, and the scanout step reuses the exact `ADDFB2`/`SETCRTC` sequence
`userland/drmsmoke/src/main.rs:362-425` already proves.

**The loader stays unshipped.** Measured: the ICD exports only
`vk_icdGetInstanceProcAddr`, `vk_icdNegotiateLoaderICDInterfaceVersion` and
`vk_icdGetPhysicalDeviceProcAddr` — no `vkGetInstanceProcAddr` — so it can never stand
in for `libvulkan.so.1`. `vkrender` bootstraps the way `vktest` does and resolves
device entry points via `vkGetDeviceProcAddr`. When stock binaries eventually matter,
build the real Khronos loader (Alpine packages it; the ICD manifest is already staged
at `/usr/share/vulkan/icd.d`) rather than a forwarding shim, which gets the
dispatchable-handle magic subtly wrong.

**Every WSI path is blocked on one kernel gap.** `vkGetMemoryFdKHR` →
`virtgpu_ioctl_prime_handle_to_fd` → our intercept at `kernel/src/syscall.rs:6049`,
which resolves handles only via `dumb_buffer_phys_order` and so returns **EINVAL for
any Venus blob**. That kills `VK_KHR_display`, `VK_EXT_headless_surface` and the
Wayland dmabuf path alike — and is precisely why offscreen rendering can work while WSI
cannot. A second blocker: Mesa 25.3.6 defines `SIMULATE_SYNCOBJ`/`SIMULATE_SUBMIT`
unconditionally, and `sim_syncobj_create` mints its signalled fd via an execbuffer with
`size=0, command=0` plus `FENCE_FD_OUT`, which we reject at two layers
(`drivers/src/drm_device_interface.rs:2941`, `drivers/src/virtio_gpu.rs:1856`) and
never write `fence_fd` back. Ordinary `vkQueueSubmit` avoids this by riding the ring
with `ring_seqno`.

For `VK_KHR_display` specifically, our atomic property model is nearly sufficient —
plane 30 has all ten required properties and CRTC 1 has `MODE_ID` and `ACTIVE` — but
the connector lacks **`DPMS`**, and `find_properties` fails the whole enumeration on
it. Note also that we return `Ok(0)` for `AUTH_MAGIC` and never gate `SETCRTC` on DRM
master, so a Vulkan client would silently fight cosmic-comp over the single CRTC; any
direct-KMS demo must run with COSMIC stopped.

**Ordered plan:** (1) `run-qemu.sh --venus` — patch ready, see below; (2) `vkrender`
subtests with readback assertions, ~600 lines C built like `vktest`; (3) stage it in
`scripts/mkfs-f2fs-populated.py`, two lines; (4) a `--present` dumb-buffer blit, ~80
lines; (5) a wave harness and archived proof. Then, as a **separate** wave:
`PRIME_HANDLE_TO_FD` for blob handles, zero-size execbuffer with `FENCE_FD_OUT`
returning a signalled `eventfd2(1)` (~40 lines, correct because `submit_3d` is
synchronous and Mesa only `poll(POLLIN)`s the fd), and an M4 Wayland client. **Riskiest
step is (2), at subtest 0** — the first `vkQueueSubmit` and wait, in Mesa's `vn_ring`
relax loop. This lane has been bitten there twice already (`75b32e3` clock
granularity, `fb398c7` nanosleep truncation), both only under sustained ring traffic;
subtest 0 is deliberately shaderless so the ring is the only suspect.

**Step 1's patch is prepared and verified** at
`~/code/leandros-artifacts/notes/m9-m3-vulkan/run_qemu_venus.patch` (52 insertions, 1
deletion; `git apply --check`-clean at `e4a53c2`, round-trip verified, and confirmed to
stack with the memfd and small-fixes patches in any order). It adds a `--venus` flag
plus `LEANDROS_VENUS=1`, setting `virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G` with
`-display egl-headless` on both arches, with `exit 1` guards and reasons for macOS,
`--raspi4b` (that branch attaches no GPU at all), a missing `virtio-gpu-gl-pci`, and
`-nographic` in `QEMU_EXTRA_ARGS` — turning the documented silent `-display` override
into a diagnosable error. **`-vga none` is dropped only under `--venus`, not
unconditionally**: the design proposed an unconditional drop, but our default x86_64
path uses `virtio-vga`, which *is* a VGA device, so dropping it there would leave two
VGA devices contending for the legacy ports — the archived working run had
`virtio-gpu-gl-pci`, a non-VGA device, making q35's implicit std-VGA the only one. The
second `-vga none` at the direct-boot branch is deliberately left alone.
Default-path-unchanged was proven locally without launching QEMU, by stubbing
`qemu-system-*` to answer only the `help` probes and diffing base-vs-patched output
across both arches and both boot modes, on macOS and under a faked-Linux environment:
4/4 identical in both.

**Still to verify** on the Linux box: that `run-qemu.sh --venus` reproduces venustest
68/68 and vktest 0 failures on both arches; that OVMF still gets a GOP from the default
std-VGA under `--venus` (the one assumption the `-vga none` decision rests on and
cannot be checked on the Mac); and a default-path regression run of both suites.

**Linux-box tree state (trap).** That checkout is on a **detached HEAD** with two
stashes, and **`stash@{0}` must not be blind-popped**. It holds 6 files but only 3 are
wanted (the AF_INET work, which has since landed as `26eebf0`); its `arch/*/src/timer.rs`
are now identical to what landed, and its `kernel/src/syscall.rs` is **older** than
current HEAD — popping it would revert `4085b7f` (nested-epoll readiness). Re-land from
`~/code/leandros-artifacts/notes/m9-af-inet-loopback/af_inet_loopback_verified.patch`
instead. A raw copy of that stash also exists at
`/home/forain/linux-tree-preexisting.patch` on the box.

### 2. `PRIME_HANDLE_TO_FD` rejects Venus blob handles

`kernel/src/syscall.rs:6049` resolves handles only through `dumb_buffer_phys_order`,
returning EINVAL for any Venus blob, which blocks `vkGetMemoryFdKHR` and therefore
every Vulkan WSI path. Pair it with the `SIMULATE_SYNCOBJ` gap: Mesa's
`sim_syncobj_create` submits a zero-size execbuffer with `FENCE_FD_OUT`, rejected at
`drivers/src/drm_device_interface.rs:2941` and `drivers/src/virtio_gpu.rs:1856`, with
`fence_fd` never written back. Both are needed before any WSI; neither blocks offscreen
rendering. Also note the connector's missing `DPMS` property, which fails
`VK_KHR_display` enumeration outright, and that `AUTH_MAGIC` returning `Ok(0)` with no
master gating on `SETCRTC` lets a direct-KMS client fight cosmic-comp for the CRTC.

### 3. `driver.py` has no GL path at all

`.claude/skills/run-leandros/driver.py:_build_cmd` hardcodes its own QEMU command line
and never calls `run-qemu.sh`: plain `-device virtio-gpu-pci` on aarch64 (not even
`-gl`), `-vga none -device virtio-vga` on x86_64, and `-display none` on both. So the
`run-leandros` skill cannot exercise GL, let alone Venus, and the `--venus` flag does
not reach it. Adding a `venus=True` mode is strictly larger than the flag —
`-display none` must become `egl-headless`, and it is unknown whether the monitor
`screendump` capture the whole skill depends on still works under `egl-headless`. That
one question gates both this and M3's scanout step.

### 4. memfd burns a tmpfs slot per call — fix prepared, needs an in-flight refcount

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

### 5. `tmpfile_owner_of` does not canonicalise pid to TGID

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
item is included in the item 4 patch above, but it is an independent defect worth its
own line — it breaks threaded memfd users generally, not just this one.

### 6. Primary-plane recomposite (FB_DAMAGE_CLIPS is the instrument, not the fix)

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

### 7. `listen()` twice returns EINVAL — fix prepared

`handle_listen` matched only `SockState::InetBound`, so a repeat call fell to
`_ => err_reply(-22)`. The fix adds one arm, `SockState::InetListening { .. } => return
ok_reply()`, before the fallthrough. **An early return, not a re-run of the path** —
falling through to `listen_on()` would add a second pair of smoltcp sockets on the same
port and orphan the handles the first listen stored, silently dropping any half-open
connection: it would return 0 and then never accept.

**No backlog is stored, deliberately.** `SockEntry` has no backlog field and the
parameter is already `_backlog`, ignored on the *first* listen too, because smoltcp has
no accept queue — `accept_on` takes the listening socket over and arms a replacement,
so the effective depth is 1 regardless. Storing a number nothing reads would fake a
knob; returning success matches Linux's observable behaviour for any program that
cannot read the state back, and we expose nothing `SO_ACCEPTCONN`-adjacent.

Verification: a new `scmtest` subtest `inet_listen_twice` is in the patch, registered
after `test_inet_loopback_tcp` in the same idiom. It asserts listen-before-bind still
gives `rc=-1 errno=22` (so the fix cannot degenerate into "always succeed"), that a
repeat `listen(srv,16)` returns 0, and that connect+accept still complete on the same
listener afterwards — the last is what catches a fix that re-arms and orphans handles.
**`scmtest` 26/0 → 27/0**, or **→ 29/0** if the memfd patch's two subtests land first.

### 8. `unused variable: port` in `handle_close` — not a leak, fix prepared

**Measured, it is a warning and not a port leak.** `alloc_ephemeral_port`
(`servers/net/src/lib.rs:442`) is the only allocator and derives "free" purely from live
table state (`t.socks.iter().any(|s| s.in_use && s.bound_port == p)`); there is no port
bitmap or pool. Every arm of `handle_close`, including the `_` catch-all and the
`UnixConnected` relookup path, stores `SockEntry::empty()`, which zeroes `bound_port`
and clears `in_use` — so **clearing the slot is the release**, and exhaustion is
impossible by construction. The patch therefore *removes* the dead binding rather than
renaming it to `_port` (a dead read invites someone to re-add it) and leaves a comment
recording why no release is needed. Verification is just that the warning is gone with
no new ones, and `scmtest` unchanged.

### 9. Delete the unreachable `init-server` crate

The scope is larger than this item previously stated. **`init-server` is a real
dependency in `kernel/Cargo.toml:34`, so all 2653 lines compile into every kernel
build**, while `init_server::` appears in no Rust code anywhere — the only external
mentions are the stale doc comment at `kernel/src/init.rs:4`, the workspace member
list, that Cargo line, `Cargo.lock`, and a README row describing it as "PID-1: server
bring-up, mounts, getty loop", **which is false** (that is `kernel/src/init.rs` plus
userland `/bin/init`). `init_main` is the crate's only public entry point, so the whole
crate is unreachable — an in-kernel shell, ~40 coreutils and the smoke tests, not just
`run_posix_tests()`.

**Wiring it in is not an option as written:** `init_main` is `-> !` and ends in
`run_shell()`, an in-kernel serial shell that never returns, so calling it would
*replace* the real boot path (initrd → `/bin/init` → getty → login → `start-cosmic`),
not augment it.

**Nothing is worth salvaging**, checked rather than assumed: the `t_*` tests call
`net::handle()` and `vfs_*` directly with kernel-space pointers, bypassing the syscall
ABI entirely, so they structurally cannot cover what the userland binaries cover.
`t_af_inet_loopback` is strictly weaker than the `inet_loopback_tcp` landed in
`26eebf0` (fixed port 9999, no `getsockname`, no reverse direction); the rest
duplicates vfstest/scmtest/memtest; and the only kernel-internal candidates,
`t_buddy_alloc` and `t_heap_end`, are one-line smoke checks of paths every boot
exercises. The patch removes the crate, the workspace member, the kernel dependency,
both `Cargo.lock` entries and the README row, and rewrites the stale doc comment.
Recoverable at any time via `git show 905148f:servers/init/src/lib.rs`. Verification:
both arches link with the crate gone, `grep -rn init_server .` is empty, serial output
is **unchanged** to the login prompt (nothing in the crate ever printed), and the full
suite is at baseline on fresh images.

### 10. AF_UNIX `listen()` is lax in the opposite direction

The AF_UNIX arm of `handle_listen` is an unconditional `ok_reply()` — a repeat listen
already succeeds, but so does `listen()` on an unbound or already-connected AF_UNIX
socket, where Linux answers EINVAL. Found while fixing the AF_INET side (item 7) and
deliberately **not** changed there: tightening it alters behaviour for every AF_UNIX
server on the system (cosmic-comp, busd, tokio) and could not be validated in a
read-only session. Needs a live COSMIC session to land safely.

### 11. No TIME_WAIT — ports are instantly reusable

`handle_close` calls `socket_set.remove()` immediately, so a closed TCP port can be
rebound at once where Linux would hold it in TIME_WAIT. A divergence, not a leak, and
low priority — but it is the kind of thing that makes a server restart behave
differently here than on Linux.

### 12. Deferred work and known limitations

- **Doom does not link relibc.** `../doomgeneric/Makefile.leandros` links
  `userland/target/<arch>-unknown-none/release/libleandros_libc.a`, whose allocator is
  `userland/libc/src/mem.rs` — a ~20-line **bump allocator over `brk(2)`** with no free
  list, no dlmalloc and no `mmap` path. The retired malloc-hang item (deleted
  2026-08-06 — Doom now runs both arches on fresh images) had blamed "relibc's
  dlmalloc or its `brk`/`mmap` glue" and nominated `04c80cd` ("give relibc's C sources
  a cross compiler") as the likely fix; **neither could ever have been right**, since
  Doom never touches relibc. Worth stating plainly so the next person debugging a Doom
  allocation does not start in relibc.
- **doomgeneric's zone default is 4 MiB, not 16.** `DEFAULT_RAM 4`; the 16 MiB case is
  reachable only via `-mb 16` (and that forced case also passes: `zone memory:
  0x33e008, 1000000 allocated for zone`).
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
- **Harness gotcha: `~/code/leandros-artifacts/m8_cursor.py` picks its "busiest
  window" by `curs_mv` delta.** That is identically 0 on the legacy KMS path (no cursor
  plane exists to move), so it silently prints a degenerate `1.00 flips/s` for a
  legacy-path control instead of erroring. Key the window on `evpush` (see the
  diagnostics table in Standing context) instead — it is nonzero on both paths whenever
  pointer motion actually reached the guest ring.

---

## Housekeeping

- Untracked disk-image backups at the repo root
  (`f2fs-data0-aarch64.img.12h15-orig`, `.full-rebuild`, `.m7z2-orig-backup`,
  `f2fs-data0-x86_64.img.m7z2bak`) and `ports/busd/.work/` are now gitignored
  (`f2fs-data0-*.img.*`, `ports/*/.work/`); delete them by hand when no longer needed.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at
  exit 144.
