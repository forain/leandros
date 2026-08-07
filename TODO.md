# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-07** (`b8ff2f6`), after a second same-day wave
that landed three commits, **closed the present half of item 2 with an actual photograph**,
emptied "Prepared but not landed" for the first time, cleared three deferred-list entries,
and corrected two more recorded claims.

**Landed in this wave.** `9d73b43` adds `drmsmoke --hold`, which paints a deterministic
two-colour frame and keeps it on the scanout; it is what finally photographed the DRM
present path on both arches. `cc82924` lands the long-prepared `driver.py --venus` mode.
`b8ff2f6` moves DRM page-flip event timestamps off the 100 Hz tick onto the interpolated
`arch_monotonic_ns()` clock, with `drmsmoke`'s new `FLIP_TS_SUBTICK` as the permanent
detector, verified by mutation (control `cd51110d`, mutant `f19b6a35`, restore `cd51110d`
byte-identical).

**The lesson of this wave is that two of four "open" items were never work.** Item 1 is
explicitly a finding whose own text says the fix is undecided and warns against fixing it
kernel-side; item 3 is killed as an M4 route by measurement with nothing scheduled. Acting
on either would have been inventing scope. The real remaining work was one item's first half
plus a prepared patch and a handful of deferred-list entries — **read what an item says it
is before scheduling it**, because a four-row table implies four tasks and this one held two.

**A sizing correction that generalises.** Item 2 asked for "a standalone, Vulkan-free
dumb-buffer present tool". No new tool was needed: `drmsmoke` already walked the whole path,
and the two genuinely missing pieces were about the *photograph* (a checkable pattern, and
not tearing down before capture), not the present. The item had been written as if from
scratch. **Before sizing a task here, check whether an existing test binary already covers
the path** — this file has now over-sized work at least once by not looking.

**Landed on this Mac,** in order. `a85e209` corrects the aarch64 `ATTR_DEV`/`ATTR_NOCACHE`
comments to the measured flat `MAIR_EL1 = 0xFF` — comments only, no code. `f43a79a` bounds
the `[DRM-SRV] mmap` trace to the first two per cache type, 146 lines per session down to
2, with the gate confirmed live under `drmsmoke`. `cec0a04` makes `driver.py` create
`aarch64_vars.fd` and makes `cmd_start` poll `proc.poll()` inside the socket-wait loop, so
a dead guest exits non-zero instead of printing `QEMU started`. `65fb20c` unifies handle
retirement behind `gem_handle_delete(handle, open_id)` for both `GEM_CLOSE` and
`MODE_DESTROY_DUMB` — `open_id` was already a `handle_ioctl` parameter, so the whole fix
was one signature and one call site — plus two new `venustest` guards. `5ce8af2` brings up
`IA32_PAT` on x86_64 across three files under `arch/x86_64/src/`. `73258ea` appends four
`[DRMSTAT]` census fields.

**The blob half of the dmabuf lifetime fix is exercised, and the fix is proven by
mutation.** On the Linux box `venustest` is **106/0 on both arches** pre-guards, with all
nine phase-6 blob assertions emitting and passing on *each* arch and the gate
`phase6_guest_blob_created` passing — that gate is the thing that distinguishes "the blob
assertions ran and passed" from "the blob assertions were silently skipped", which is the
only failure mode that mattered. Removing the blob arm's `o.refs` increment in
`prime_export_acquire` reproduced all three predicted signals on x86_64:
`phase6_objs_survive_close` FAIL (`expected 1 live objects, got 0`),
`phase6_payload_survives_close` FAIL (`payload lost at offset 0 - the fd read RECYCLED
memory`), `phase6_mmap_of_fd_still_coherent` FAIL, and `[DRM] bo refcount underflow
obj=0x0000001B` at `close(fd)`. No panic — **wrong values**, exactly as the item predicted,
because the frames stay HHDM-mapped and merely belong to someone else. Control, mutant and
restore kernel md5s were `10496aac…` / `46b3951f…` / `10496aac…`, restore byte-identical to
control, with the image and the `venustest` ELF held constant.

**The handle-retirement unification is verified, not merely landed.** Box baseline
**108/0 on both arches** (106 plus the two new guards), cross-footed against
`--- venustest done, failures = 0 ---`. Both guards were falsified by mutation. Reverting
`std_handle_destroy_dumb` to call `free_dumb` gives
`phase6_destroy_dumb_releases_blob_handle: FAIL — DESTROY_DUMB on a blob handle: live
objects 1 -> 2 -> 2 (want back to 1)`, and two further phase-6 subtests fail as well — the
*same* leaked object seen three times, which is what shows the leak is permanent rather
than a timing artifact. Reverting `free_blob_owned` to `free_blob` gives
`phase4_other_open_destroy_dumb_refused: FAIL` with `[DRM] RESOURCE_INFO: unknown
bo_handle=0x00004017`, and `phase4_bo_survives_close_by_other_open` fails alongside it as
the expected cross-check. md5s: control `40f4da1e…`, mutant A `9e0bfb68…`, mutant B
`525b6e3a…`, both restores back to `40f4da1e…`. **Scope limit:** the mutations ran on
x86_64 only; the baselines ran on both arches. The retirement path is arch-independent, so
that split is a cost saving and not a gap, but it is stated rather than assumed.

**x86_64 `IA32_PAT` is up, the divergence it was built for was real, and WC is worth
107×.** All three of that item's pre-committed checks are satisfied. (a) `[ARCH] IA32_PAT
before=0x0000010500070406 after=0x0000010500070406 wc=1`, reproduced across boots and
**identical on the Linux box**, which converts the static `BOOTX64.EFI+0x42f34` decode into
a runtime read on two independent machines. (b) **The divergence was live, not inferred.**
`PD[0] = 0x000000008000108b` — a 2 MiB leaf with PAT bit 12 set and PWT set, PCD clear,
selecting PA5, which is WC on the BSP and WT on every AP. `arch::init`'s `NO_CACHE` re-map
failed on **1536 of 2025 pages**, because `map_4k` cannot split Limine's huge pages —
precisely what that loop's own comment guessed before discarding the return value. (c) The
win: a 1 MiB `memcpy` into a host-visible blob, WC median **59.0 µs** against UC-
**6324.8 µs**, n=30 each, distributions completely disjoint (worst WC 85.8 µs, best UC
6243.6 µs), against a guest-RAM control flat at ~3.9 µs. WC also beats write-back into the
same blob by 1.11×.

**Carry this caveat forward, because it decides what the 107× is worth.** A plain
`blob_id=0` HOST3D blob is answered `map_info=0x01` (CACHED), so the kernel does not set
the uncached hint and maps it **write-back**. A naive benchmark therefore never touches the
`WRITECOMBINE` path at all and would have reported the patch inert — which was one of the
two outcomes the item explicitly predicted, so the false negative would have arrived in
exactly the expected shape and been believed. Only Mesa's fence-feedback slot answers
`0x03` in a real session. Both comparison boots carried identical forced-WC reporting so
`PAT_WC_READY` was the sole variable, and both logged `map_info=0x03 -> uncached`. **This
also bounds how much of the 107× the system realises today: one slot, not every blob.**

**The primary-plane over-damage item is closed as a negative result, and its central
arithmetic was a coincidence.** Measured on the box at **1920x1080** (not the Mac's
1280x800). The idle positive control validated exactly — 1920×32 = **61,440 px per
present** across 72 intervals, `dmg_nrects − dmg_rect = 0`, and the identity
`dmg_full + dmg_rect + dmg_skip == atomic` exact. Under motion,
**`dmg_nrects / dmg_rect = 1.0000 across all 340 presents`**: never more than one clip
rect, ever. The dumps show `n=1` and a single rect `(0,32)-(1920,1079)` = 1920×1047 =
2,010,240 px, 96.9% of the output. That is *not* the `dmg_full` fallback, which emits
exactly `output_geo` = 2,073,600 and was seen only at bring-up (`dmg_full` was 0 throughout
motion). 4,524 pointer moves in 80 s = 56.5/s, `evpush/s = 228` at peak. **The tiled shaper
never ran, and the pre-shaper damage set is genuinely large.** The hypothesis that a
handful of small rects were being inflated into a million pixels is excluded, so there is
**no upstream smithay bug here and no reproducer to file**. The old item's 4x8 tile
arithmetic was **a coincidence, not a mechanism**: `1280 × 775 = 992,000` and
`1280 × 767 = 981,760` exactly, so the Mac's burst values were single full-width rects of
that shape, not sums of 31 tiles. The grid is real (`NUM_TILES = 4`, 320×100 at 1280x800)
and never executed. Its own pre-committed discriminator was also incomplete: at the smithay
revision cosmic-comp uses (`efeb597`), `n=1` has **two** producers — the
`in_damage.len() == 1` passthrough (lines 57-60) and the bbox shortcut (lines 84-89, gated
at `MAX_DAMAGE_TO_DAMAGE_BBOX_RATIO = 0.9`) — and the kernel cannot tell them apart. The
conclusion is unchanged because both imply a large input. **Ceiling, kept explicit:** this
is the shaper's *output*. The input stays inferred, because `release_max_level_info`
compiles out the `trace!` calls; what is measured is `n=1` and the geometry, what is
inferred is that the input held a rect of ≥ 1,809,216 px.

**Corrections to the record, which live here because `git log` cannot be edited.**

1. **`scmtest`'s baseline is 32/0, not 31/0.** This is not drift and nothing
   double-reports: `97a979e` left 30 subtests, `fe411ff` added `tcp_time_wait` for 31, and
   `055745f` added `unix_listen_strict` for 32. The `listen()` lane branched from
   `97a979e`, correctly measured 30 → 31 against an ancestry that had no `tcp_time_wait`,
   and was never re-measured after being rebased onto `fe411ff`. Two individually correct
   31s from two concurrent lanes. The live PASS-name list confirms 32 including both names,
   and `main` counts 32 dispatches in `scmtest`'s `main`. **General lesson, and the reason
   this is worth a paragraph: a test-count baseline measured on a branch is only valid
   against that branch's ancestry, and a rebase silently invalidates it.** Re-measure after
   rebasing, or quote names rather than counts.
2. **`venustest`'s baseline is 108/0** (106 before this wave's two guards). The recorded 91
   was measured at the box's `a0325c6`, where `venustest/src/main.rs` contained **zero**
   `phase6` assertions — phase 6 arrived with `49399f9`. Sixteen new report sites, one
   (`phase6_open_card0`) reachable only on the failure path, so fifteen emit: 91 + 15 =
   106, plus 2 = 108.
3. **`waittest` has 4 subtests and scores 4/0 or 3/1**, the failure being the known
   `wait_on_process_group` race. The recorded "5/0 or 3/2" is unreachable against the
   source, and the cause is now known: `waittest` prints a trailing `WAITTEST: PASS`
   summary line *on top of* its four subtests, so a naive `grep -c ': PASS'` reads one
   high. Record the explanation and not just the number — the same extractor may still be
   in use elsewhere.
4. **The handle-retirement item's severity claim was wrong.** It was not one leaked object
   per composited frame. cosmic-comp exports **per buffer at allocation, not per frame**,
   measured in `~/code/leandros-artifacts/notes/m9-dmabuf-lifetime/mac-verify.md` §5.3-5.5:
   38 samples over 185 s showed 5 dumb creates, 5 PRIME exports, 1 free, then frozen, with
   the panel clock still ticking. Projected post-Stage-3 leak is ~5 objects per session, not
   thousands. A measurement sitting in the same artifacts directory already contradicted the
   item and was not read.
5. **Three stale source pointers**, all corrected in this file: `DRM_STATS` is at
   `drivers/src/drm_device_interface.rs:1734`, not `:1344`; `open_may_reach` is at `:1237`,
   not `:1093`; the page-flip event timestamps are built at `:1761-1762`, not `:394,398-400`
   (which is now `GPU3D_DEBUG` helper code). The recorded `std_handle_destroy_dumb`
   `:2833` was also stale — that offset is `std_handle_get_blob` — and the function has
   moved again since. **Verify every line number against the tree before writing it here;
   this file has now shipped stale ones twice.**
6. **Only two TODO-citation violations existed, not three.**
   `userland/vfstest/src/main.rs:1` and `userland/f2fstest/src/main.rs:1` had already been
   fixed by `033f3d0`, an ancestor of HEAD. The `driverpy_venus.patch` citations were real
   and are now removed. **One new violation exists**, introduced by `73258ea`:
   `drivers/src/drm_device_interface.rs:1787` cites "item 9", and it repeats the per-frame
   severity claim that correction 4 refutes. Two wrongs in one comment line; it is a
   one-line fix, deliberately not folded into this documentation commit.
7. **The two trees were never divergent as recorded.** The box was already at `a1568ec`
   when this wave began, not at `a0325c6`, with `git patch-id --stable` matching on both
   machines; it has since been synced forward. The standing lesson survives the correction
   and is the reason the divergence was misread in the first place: **compare by
   `patch-id`, not by SHA.** These two trees have twice received the same change under
   different SHAs.

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment *unmodified* (source: `../cosmic-epoch`)
on both x86_64 and aarch64 under QEMU. No COSMIC source patches; build-configuration
flags (`--no-default-features`) are allowed. Everything beneath COSMIC — kernel, libc,
system libraries, daemons — is ours. **This constraint stays load-bearing**: the missing
dmabuf global is behind cosmic-comp's `!is_software` gate (item 3), and the reachable
outcome there is a measurement, not a patch. The one place it looked like it would force an
upstream bug report — the primary-plane damage — turned out to have no upstream bug at all.

**Where it stands.** The desktop runs on both arches: cosmic-session → cosmic-comp on
KMS/softpipe → busd → cosmic-bg + cosmic-panel renders a wallpaper plus a full-width
panel bar with an embedded Wayland client, clock ticking. Remaining desktop work is
quality and performance, not bring-up. Vulkan runs **and presents**: `vkrender` executes
fill-buffer, compute and graphics work, `vkswap` drives a headless-surface swapchain to
`vkQueuePresentKHR -> VK_SUCCESS`, and `vkrender --present` puts a rendered image on a
real DRM scanout.

**Suite baselines.** On fresh images with `vfstest` run exactly once per image, both
arches: vfstest **36/0**, scmtest **32/0**, drmsmoke **22/0**, wakepolltest 10/0,
forktest 3/0, epolltest 9/0, polltest 6/0, sigtest 6/0, timertest 6/0, memtest 4/0,
idletest 2/0 (`IDLE_CPU_US 0`), evtest2 8/0. `waittest` has **4** subtests and is **4/0 or
3/1 on either arch** — a pure timing race in `fork` → child `setpgid(0,0)`+`_exit` →
parent `waitpid(-pid)`, measured on pristine kernels too; either result is acceptable on
either arch and the arch asymmetry in any single wave is noise. Note that `waittest` also
emits a trailing `WAITTEST: PASS` summary line, which a `grep -c ': PASS'` will miscount as
a fifth subtest. On a **Venus host** (the Linux box, `--venus`): `venustest` **108/0 both
arches**, `vktest` 14/0, `vkrender` **51/0** with `s2_checksum = 0x02C0FDC5` pinned across
x86_64/KVM, x86_64/TCG and aarch64/TCG, `vkswap` **21/0** (x86_64). `vkrender` under KVM
does **not** need `VN_PERF=no_fence_feedback`; that dependency died with `18a7a9f`.

**A Mac `venustest` run is worth nothing, in either direction.** QEMU 11.0.2 on macOS has
**no blob-capable virtio-gpu device at all**: `virtio-gpu-pci,blob=on` is refused with
*"need rutabaga or udmabuf for blob resources"*, and neither `virtio-gpu-gl-pci` nor any
rutabaga variant is compiled in. `VIRTIO_GPU_F_RESOURCE_BLOB` is never advertised, so no
blob BO can be created and nothing downstream of one can be exercised. A Mac `venustest`
reports **42 lines, 11 PASS / 31 FAIL**, byte-identical on patched and unpatched kernels.
Do not compare that against the box's numbers and conclude anything. Everything blob-,
HOST3D- or Venus-shaped goes to the box.

**The Linux box.** `forain@172.16.158.150`, `/home/forain/Projects/leandros`, EndeavourOS
(**Arch, not Debian**), virglrenderer 1.3.0, QEMU 11.0.1, Mesa 26.1.3, host GPU a Ryzen 9
7950X iGPU (RADV RAPHAEL_MENDOCINO). aarch64 there needs `-cpu max,lpa2=off` (the Limine
11.4.1 FEAT_LPA2 wedge). Sync by push/fetch over SSH between the two machines; **never**
push to `origin`, which is untouched at `6a0eb0c`. Compare the two trees by
`git patch-id --stable`, not by SHA. The working `--venus` device line is
`virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless`; `-nographic`
silently overrides `-display`.

**cosmic-comp offers no dmabuf to clients here — measured, not inferred.** A live aarch64
session advertises 54 globals on `/run/user/0/wayland-1`, identical across three passes 30 s
apart; `zwp_linux_dmabuf_v1`, `wl_drm` and `wp_drm_lease_device_v1` are all absent, and no
`wayland-1-card0` socket exists, so `create_socket` (`cosmic-comp/kms/socket.rs:31`) was
never called and `is_software` is true. **Scope:** absent *in this configuration* —
software EGL, forced because the macOS host has no EGL, so `virtio-gpu-gl,venus=on` is
unusable and the guest has no hardware GL. It would flip only if the guest gained a
non-software EGL device. Full report and controls:
`~/code/leandros-artifacts/notes/m9-crossopen-dmabuf/stage0a-wl-globals.md`.

**The primary plane's over-damage is upstream-correct behaviour, not a defect.** Closed by
measurement this wave (see the header). What survives for future work: damage tracking
demonstrably works when idle (exactly the panel-bar rectangle per present, for minutes at a
time); the age hypothesis is refuted on two independent lines, from source
(`Swapchain::acquire`, `allocator/swapchain.rs:154-181`, calls `create_buffer` only inside
`if free_slot.buffer.is_none()`, and cosmic-comp holds at most two slots) and from data;
and under motion the shaper emits exactly one rect covering ~97% of the output because its
*input* is that large. `DrmDevice::present_damaged` (`drivers/src/drm/device.rs:411`) copies
only the sub-rectangles a `FB_DAMAGE_CLIPS` blob names, which is the kernel-side defect that
was worth fixing regardless — a skipped primary used to cost a full-screen scale plus a
full-screen `TRANSFER_TO_HOST` and `RESOURCE_FLUSH`. Judge that change on that, not on
flips/s; there is no perf headroom to recover.

**`RUST_LOG=trace` cannot read smithay's own damage-tracking decisions.**
`cosmic-comp/Cargo.toml:61-62` sets `release_max_level_info` on `tracing`, so `trace!`
calls are compiled out of the release build and the feature ceiling cannot be raised
additively. Kernel-side counters are the only instrument, and the `FB_DAMAGE_CLIPS` blob
is the damage tracker's **verbatim** output (`PlaneDamageClips::from_damage`, smithay
`backend/drm/surface/mod.rs:68-100`, is a 1:1 `map` with no splitting or merging), which
is what makes the kernel-side decode a real measurement of a client-side decision. It reads
the shaper's output only; the input is not visible from the kernel at all.

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
- The launcher is a **shell script and the kernel has no shebang/binfmt support**, so
  `execve` on it fails with `Exec format error`. Launch it as
  `brush /bin/start-cosmic-leandros` (item 4).

**Kernel invariants.**

- Never touch user memory under `RUN_QUEUE` or any IRQ-off spinlock. Use
  `validate_user_buf`/`read_user_buf`/`write_user_buf`. A re-entrant `RUN_QUEUE`
  deadlock from exactly this froze all four vCPUs once (fixed in `82d0cc3`).
  **Trap for next time** (`26eebf0`, `handle_send`): `read_user_buf` alone does not
  fault a lazy page in — it resolves through `virt_to_phys`, which returns `None`
  instead of faulting, and `sys_sendto` never calls `prefault_user`, only
  `validate_user_buf`. Either pair it with `prefault_user` (private to the syscall
  crate) or hoist the copy above the lock so the fault happens with nothing held.
- **Anything sampled from an IRQ-context hook must use `try_lock` and must report a missed
  sample as missed.** `drm_tick` (`drivers/src/drm_device_interface.rs:1826`) runs at
  100 Hz in IRQ context; a blocking `.lock()` there deadlocks the instant the tick
  interrupts a thread already holding the same mutex from an ioctl — the `RUN_QUEUE` freeze
  shape, which wedges every CPU with no panic. `bo_census` (`:1801`) is the worked example:
  `try_lock` only, and contention yields `u64::MAX` rather than 0, because a zero would be
  indistinguishable from "every object was freed", which is the exact conclusion the
  instrument exists to support or refute.
- **The kernel is softfloat on both arches and must stay that way.** The EL0 trap
  frame saves no vector state, so any kernel code LLVM lowers through a vector
  register lands on the interrupted thread's. Both kernel target JSONs disable the
  vector units; `cpu_switch_to` is the single deliberate exception and scopes the
  extension with `.arch armv8-a+fp+simd` … `.arch armv8-a`. Six items across the previous
  session trace back to this clobber, directly or as the cause that retired them.
- **A borrowed VMO's page list is immutable**, and **an exported dmabuf fd keeps its DRM
  object alive** (`3dbba0c`, `49399f9`). Together these close four hazards measured *live*,
  not theorised: an unpatched kernel returned a valid mapped address for a page past the
  frames the DRM layer lent it, accepted an 8-byte `write()` into it, *succeeded* at
  shrinking a borrowed frame list (order-0 frees out of an order-N buddy block), and let a
  `GEM_CLOSE` recycle memory an open fd still named. The refcount is one per gem handle and
  one per exporting `TmpVmo` slot. **Failure direction, worth knowing before debugging it:**
  the lifetime fix can only make buffers live *longer*, so its failure mode is a leak where
  the previous failure mode was memory corruption.
- **Handle retirement has exactly one path.** `gem_handle_delete(handle, open_id)`
  (`drivers/src/drm_device_interface.rs:3283`) serves both `GEM_CLOSE` and
  `MODE_DESTROY_DUMB`, dropping one reference regardless of which registry minted the
  handle. Gallium's kms-dri winsys releases *every* imported `pipe_resource` through
  `DRM_IOCTL_MODE_DESTROY_DUMB` rather than `GEM_CLOSE`
  (`src/gallium/winsys/sw/kms-dri/kms_dri_sw_winsys.c:288-296`) — upstream's shape, not a
  Mesa bug — so any new retirement route must go through the same function or it will miss
  imports.
- Release builds only — debug builds crash early. Test **both** arches in QEMU after
  every change. Minimum Limine revision is **6**, never downgrade.
- Regression images must be freshly regenerated — run vfstest **exactly once** per
  freshly generated image. A dirty f2fs image produces phantom failures
  (`chroot_confines_symlink_resolution`, `xattr_list_tmpfs`, `xattr_list_f2fs`). The
  historical aarch64 `xattr_list_f2fs` red has not appeared on either machine across two
  sessions, consistent with it being that artifact and not an arch bug.
- **A guard test must be shown to fail with its guard removed, or it is certifying a
  hazard it never checked.** Every guard landed this wave was falsified by mutation with
  control/mutant/restore md5s recorded and the restore byte-identical to the control; that
  is the bar. A test that cannot fail and an instrument that cannot report failure are the
  same defect.
- **Subtest comments and source comments must not cite TODO item numbers.** This file gets
  renumbered as items land — every citation has drifted within a day, every time. Point to
  the defect or the commit instead; those do not move. **One violation is outstanding**,
  found by `grep -rn 'TODO\.md item\|TODO item [0-9]'` at reconciliation time:
  `drivers/src/drm_device_interface.rs:1787`. It also states the refuted per-frame severity.
  (`userland/vfstest/src/main.rs:51` names an item by *topic*, "extended attributes", not by
  number, and is fine — the rule is about numbers, which move.)

**Memory attributes, measured rather than assumed.**

- *aarch64.* `MAIR_EL1` arrives as a flat **`0x00000000000000ff`** under Limine 11.4.1 —
  attribute 0 is `0xFF` (Normal WB/WA) and **attributes 1..7 are all zero**, i.e.
  Device-nGnRnE. So **both** `ATTR_DEV` (index 1) and `ATTR_NOCACHE` (index 3) select
  Device-nGnRnE — not Device-nGnRE and not Normal-NC, whatever their names suggest.
  `a85e209` corrected both comments (`arch/aarch64/src/paging.rs:19` and the doc block at
  `:25-37`) so the source now agrees with the register. `18a7a9f` installs **index 2 =
  `0x44`** (Normal Inner/Outer Non-cacheable) with a read-modify-write in
  `mmu::enable_identity`, before `arch::init` maps anything and before `smp_init` snapshots
  MAIR for the APs, and prints `[ARCH] MAIR_EL1 before=… after=…`
  (`arch/aarch64/src/lib.rs:84`) so the inherited value stays visible. The aarch64
  framebuffer is therefore Device memory and is **deliberately left that way**; it works
  only because `pitch = width*4` keeps every access aligned. Use `ATTR_NORMAL_NC` for
  anything that needs non-cached *Normal* memory.
- *x86_64.* Limine 11.4.1 programs `IA32_PAT` to `0x0000_0105_0007_0406` (PA0 WB, PA1 WT,
  PA2 UC-, PA3 UC, PA4 WP, **PA5 WC**, PA6 UC, PA7 UC) — originally a static decode of a
  `mov ecx,0x277` / `wrmsr` site in `BOOTX64.EFI+0x42f34`, now a **runtime read** confirmed
  identical on two machines by `[ARCH] IA32_PAT before=… after=… wc=…`
  (`arch/x86_64/src/paging.rs:238`). `BOOTAA64.EFI` has zero such sites; only our
  direct-boot path (`kernel/src/entry_x86_64.s`, which writes EFER and nothing else) leaves
  the reset PAT. `5ce8af2` makes every CPU agree: `init_pat_bsp()` is the first statement of
  `arch::init`, `init_pat_ap()` the first statement of `smp::sched_ap_entry`, and **PA5** is
  the guaranteed WC slot (`PAT_WC_READY`), chosen because the write is provably inert on the
  Limine path — PA5 is already `0x01` there, so the read-modify-write is value-identical and
  cannot reinterpret a live translation, including Limine's own PAT-bit framebuffer mapping.
  On direct boot PA5 goes WT → WC and provably has no users, since reaching PA4..PA7
  requires the PAT bit and the 2 MiB PDEs `entry_x86_64.s` builds have bit 12 clear.
  `wc=0` means the CPU or hypervisor refused the write and we fell back to PCD/UC-, which is
  `18a7a9f`'s behaviour and not a failure.
- *The divergence this fixed was live.* Before `5ce8af2`, `arch::init`'s `NO_CACHE` re-map
  of the framebuffer failed on 1536 of 2025 pages because `map_4k` cannot split Limine's
  huge pages, leaving `PD[0] = 0x8000108b` — a 2 MiB leaf selecting PA5, i.e. WC on the BSP
  and WT on every AP, one set of physical lines under two memory types, which the SDM leaves
  undefined. The console writes through Limine's HHDM mapping
  (`drivers/src/framebuffer.rs:653`). It never bit us because WT is coherent and the console
  is idempotent.
- *WC is weakly ordered where UC was not.* That moves us toward the reference behaviour
  rather than away from it — the host explicitly asks for `VIRTIO_GPU_MAP_CACHE_WC`, so
  Mesa's Venus path is written against WC semantics on native Linux, and its ring submission
  goes through a locked atomic that drains the WC buffers — but it is worth knowing if a
  blob ever gains a new consumer.
- *MTRRs cannot defeat this on the hardware we verify on.* The blob lives in a 64-bit
  prefetchable BAR above top-of-RAM, where firmware leaves `MTRRdefType = UC`, and
  (MTRR=UC, PAT=WC) is WC. Corroborated twice, because a recalled SDM table row is not
  evidence: Linux's `arch_phys_wc_add()` adds no MTRR when `pat_enabled()`, and
  `pat_x_mtrr_type()` consults MTRRs only for WB requests. The one configuration that could
  defeat it — an old-KVM Intel host with EPT `IPAT=1` — would equally defeat `18a7a9f`'s
  already-landed UC mapping, which demonstrably works.

**Diagnostics in-tree, all `false` at HEAD** — flip to `true`, measure, flip back before
committing. `c5abb8d` shipped with `DRM_STATS` on and `c27557f` had to undo it; the rule is
not decorative. All four line numbers below were re-verified against the tree at `73258ea`.

| Flag | File | Measures |
|---|---|---|
| `DRM_STATS` | `drivers/src/drm_device_interface.rs:1734` | flips, cursor up/mv, atomic, atest, cplane, `dmg_{full,rect,skip,px}`, `blobs`, `evpush`, `bo_{dumb,dumbret,blob,bhnd}` |
| `CURSOR_DEBUG` | `drivers/src/virtio_gpu.rs:342` | cursor queue setup + selftest |
| `mm::gap2::ON` | `mm/src/gap2.rs:17` | memfd/MAP_SHARED path + frame checksum sampler |
| `pci::RENDER_DEBUG` | `drivers/src/pci.rs:99` | per-frame DRM/FB/GPU/KMS/SND serial tracing |

**Reading the `[DRMSTAT]` BO census** (`73258ea`). The four `bo_*` fields are **derived**
from map `.len()` rather than kept by hand, so they cannot drift from the maps they
describe. `bo_bhnd` sustained above `bo_blob` — more gem handles than objects — is a
**handle leak**. `bo_blob` above `bo_bhnd` is the healthy converse: objects outliving their
handles because an exported dmabuf fd still pins them. `bo_dumbret` climbing monotonically
is the **retention leak**. `u64::MAX` in any field means the sample was skipped on lock
contention, not that the map was empty. **New `[DRMSTAT]` fields go at the END of the line,
never in the middle** — `c5abb8d` inserted five `dmg_*` fields mid-line and every
position-keyed parser downstream silently reported zero for everything after them.

Measured with the flag on: **zero `u64::MAX` across 69 + 67 + 76 samples**, no hang under
four `drmsmoke` cycles, a full `venustest`, and a 200 s live COSMIC session. `bo_dumb` went
0 → 1 → 0 per `drmsmoke` cycle without accumulating; `bo_dumbret` was 0 throughout; the blob
fields were 0 because this Mac cannot make blobs. **A non-reproduction, stated as such:** the
earlier 185 s census found live dumb buffers frozen at 4, and this COSMIC run read 0
throughout — because it was still in startup when capture ended (`flips_sub=1`, `atomic=0`,
the panel still connecting to the notifications daemon). It does not reproduce the 4 and it
does not contradict it. The bounded-not-climbing property rests on the `drmsmoke` cycles,
which did exercise the path.

**Instrument reliability — read this before trusting a number.** **Nineteen** separate
instruments have now produced believable wrong numbers, or would have. They are grouped by
failure class rather than listed in order of discovery, because the class is what
generalises; the specifics are what make each one actionable.

*Class A — the instrument reported success it never measured.* Every one of these failed
**toward looking successful**, which is why they are the most dangerous class.

1. A serial `expect()` that searched backwards over an accumulated buffer and re-matched the
   *previous* command's end sentinel: every command after the first reported `rc=0` **without
   ever running**. Caught only because a log claiming `venustest` passed contained no
   `venustest` output. Take the buffer mark *before* sending, and number the sentinel per
   command.
2. `git apply --check … | head -10 && echo OK` reports success whenever `head` succeeds,
   because a pipeline's status is the *last* command's — so a patch that failed printed
   `APPLIES CLEAN` two lines below its own error text. Branch on the command itself
   (`if git apply --check …; then`).
3. `driver.py start` printed `QEMU started` over a guest that had already exited, because
   `aarch64_vars.fd` was missing. The serial log was 0 bytes, so every test read as *absent*
   rather than *failed* — in a suite that greps for PASS lines, indistinguishable from a run
   where nothing was asserted. Fixed in `cec0a04`; the lesson is that absence and failure
   must be distinguishable at the harness level.
4. **QEMU's Unix serial chardev serves one client at a time and discards whatever the guest
   emits while no client is attached.** `driver.py cmd` connects, reads for its timeout and
   disconnects, so under TCG x86_64 — slow enough for output to start after the reader has
   left — a whole command's output is lost and resurfaces mid-stream in the *next* capture.
   `drmsmoke`'s output appeared inside `scmrun.py`'s log, opening at `VERSION: PASS`, and
   `grep -c ': FAIL'` on the truncated capture returned **0**: indistinguishable from a clean
   run. Caught only because the positive control went silent on x86_64 after firing on
   aarch64 minutes earlier. Fix: hold one connection for the whole command, which
   `scripts/scmrun.py` does.

*Class B — the instrument measured something, but not the thing.*

5. The Stage 0a registry dumper, caught before it lied. The natural build — extend
   `leandros-applet` — would have enumerated **cosmic-panel's embedded server**, because the
   panel hands each applet an inherited `WAYLAND_SOCKET` fd and `connect_to_env()` follows
   it. That server advertises `wl_compositor`, `wl_shm` and `xdg_wm_base` and no dmabuf:
   **indistinguishable from the true negative being hunted, and it satisfies the "the other
   globals prove the dump worked" sanity check**. The general lesson is that *a sanity check
   can be satisfied by the very failure it was meant to exclude*, so an instrument must
   establish **which** thing it measured, not merely that it measured something.
   `wl-globals` does: it ignores the environment, globs `wayland-*` in `$XDG_RUNTIME_DIR`,
   connects by explicit path, and the socket's identity is pinned by cosmic-session's own
   `got environmental variables from cosmic-comp: [("WAYLAND_DISPLAY", "wayland-1")]`.
6. **The one-client rule applies to QMP as well as to the serial chardev.** A
   damage-measurement run injected **zero** pointer moves because a second QMP client was
   opened while a resolution probe still held the first. An abort-on-zero-moves guard now
   makes that impossible to miss.
7. A blob write-bandwidth benchmark would have measured the wrong memory type. A plain
   `blob_id=0` HOST3D blob is answered `map_info=0x01` (CACHED) and maps **write-back**, so
   the `WRITECOMBINE` path is never touched and the result reads "inert" — which was one of
   the two outcomes the item explicitly predicted, so the false negative would have arrived
   in the expected shape and been believed. Assert the memory type you think you are
   measuring, in the log, on both arms.

*Class C — the extractor was wrong about the text.*

8. A parser keyed on field *position*: `m8_cursor.py`'s regex ran from `flip_us` onward, and
   `c5abb8d` inserted five `dmg_*` fields between `flip_us` and `curs_up`, so every field
   after the insertion point silently read **0** on a patched kernel. Parse `key=0xHEX`
   pairs order-independently (`m9_analyze.py` does).
9. A `^\S+: PASS$` extractor reported `PASS=0` because the serial console emits CRLF.
10. An all-caps summary filter scored `drmsmoke` **2/22** by eating real subtests whose names
    are upper-case, like `CREATE_DUMB: PASS`.
11. `waittest`'s trailing `WAITTEST: PASS` summary line, counted as a fifth subtest by a
    naive `grep -c ': PASS'`, is the whole reason its baseline was recorded as "5/0 or 3/2"
    against a source file with four cases.
12. **`grep` over a serial log sharing a pty with QEMU's trace stream.** With
    `-trace virtio_gpu_cmd_*` and no `-D`, every guest character triggers a console flush, so
    trace lines land *between* the guest's bytes: `present_addfb2: PASS` arrived as twenty
    single characters. `grep -a "present_"` found **2 of the 10** present subtests and
    reported nothing wrong — the eight missing ones looked exactly like eight subtests that
    never ran. The same shredding broke the harness's own sentinel, so an `rc=0` run was
    reported as a harness failure. Fix: `-D <file>`, so the trace stream never touches the
    pty.

*Class D — the test could not fail.*

13. `memfd_inflight_close` as first written could not fail, because the hazard window never
    opened. The same trap was walked into and *avoided* later: a `close(0)`-consequence check
    would have been vacuous, since `sys_fcntl` short-circuits `fd <= 2` and answers
    `F_GETFD` with a hardcoded `0` without consulting the fd table.

*Class E — the number was real; the window was wrong.*

14. A count delta that looked perfect: `virtio_gpu_cmd_ctx_submit` events per renderer
    lifetime came out 6 on HEAD and 7 patched — a clean +1. The same binary three times in
    one HEAD boot gave **6, 6, 7**. Venus notifies its ring opportunistically; the count
    floats and the +1 was noise.
15. A window-selection artifact: taking the *longest* zero-`evpush` window began before
    bring-up, averaged full-output repaints into the per-present figure, reported
    126,072 px and produced a **false failure of the idle positive control**. Take the
    *tail* of a quiet period — steady state by construction, rather than by choosing
    favourable samples.

*Class F — the environment moved under the measurement.*

16. A positive control that came back showing **only the prompt**, because the read window
    raced login settle. Re-running it passed — which is itself the argument for running a
    control rather than assuming one would have passed.
17. **Commands sent to a shell ~180 s after a COSMIC session launch do not execute** (or do
    not echo): the console is saturated by session output. Any session-probing design must
    **background its work early**, while the console is still responsive.
18. `driver.py cmd`'s shell-prompt heuristic swallowing error lines on TCG x86_64, where the
    guest is slow enough for the heuristic to break early.
19. A shared-scratchpad collision: a stale foreign results directory predating the run by
    ~40 minutes, with the wrong filenames, sitting exactly where a lane's output would be
    read from. Namespace scratch output per run, and stat the mtime before reading it.

Three rules follow, and all are cheap. **Run a positive control** — send a known-failing
command (`nosuchbinary_xyz42`) as the first command of every boot and confirm the harness
reports it failing; that single step catches 1, 3, 4, 16 and 18. **Prefer a structurally
distinctive observable over a count delta** — replacing a submit *count* with a
`(payload size, flag word, ring index)` histogram settled the syncobj question
unambiguously where the count had hidden the event entirely. **Cross-foot every number
against a second, independent source** — the test binary's own `failures = N` trailer is
what caught 9, and it is what pinned `venustest` at 108/0 this wave.

**Vulkan test-binary build findings, load-bearing for anyone rebuilding them.** `-std=c11`
does **not** compile against musl — strict ISO hides `clock_gettime`, `nanosleep` and
`CLOCK_MONOTONIC`; use `-std=gnu11`. Vulkan headers need `/usr/include/vk_video` as well as
`/usr/include/vulkan`, copied to a private dir — do not point `-I` at `/usr/include`, it
shadows the target libc's headers. The container recipe **cannot build aarch64 on the box**:
no docker, and podman pulls arm64 images but cannot execute them. Cross-compiling with
`zig cc` + `musl-dyn-link.sh` works, with two gotchas — `zig cc` enables UBSan by default
(link fails on `__ubsan_handle_*`, needs `-fno-sanitize=undefined`) and its driver silently
produces a **static** binary, which cannot `dlopen` the ICD. Corrected recipes:
`~/code/leandros-artifacts/notes/m9-m3-vulkan/build-vkrender-alpine-fixed.sh`,
`build-vkrender-aarch64-zig.sh`, and `m9-vkswap/build-vkswap-alpine.sh`. The Vulkan loader
stays unshipped: the ICD exports only `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr`, so it can
never stand in for `libvulkan.so.1`. `vkrender`'s `s2_checksum` is **printed but not
asserted** unless `VKRENDER_EXPECT_CHECKSUM=0x02C0FDC5` is exported; every comparison so far
has been done by hand.

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
| 1 | A host-refused `RING_IDX` submit costs a full control-queue timeout | Finding — kernel/host | — |
| 2 | The last unproven hop is a Vulkan-free present on the non-Venus path | Feature | — |
| 3 | Cross-open dmabuf import — dead as an M4 route, alive for other reasons | Feature — deferred | — |
| 4 | Deferred work and known limitations | Mixed | — |

---

## Prepared but not landed

**Nothing.** `driverpy_venus.patch` landed as `cc82924` — it applied clean, and its device
line was verified character-for-character against `run-qemu.sh` rather than trusted. This
section is now empty for the first time; keep it that way by landing prepared patches in the
wave that prepares them, since both patches that sat here needed a rebase before they applied.

---

### 1. A host-refused `RING_IDX` submit costs a full control-queue timeout

A `RING_IDX`-routed `SUBMIT_3D` whose **command stream the host refuses to dispatch** is
never completed: QEMU defers the control-queue response to a fence it routes through the
renderer context (`virgl_renderer_context_create_fence`), that context never retires it,
and the caller pays `VirtioGpu::submit`'s full 100 M-iteration busy-spin
(`drivers/src/virtio_gpu.rs:890`) instead of receiving an error —
`[GPU] control-queue TIMEOUT, cmd=0x00000207`. Unringed submissions land on the global
timeline and retire regardless of what the host made of the bytes, which is why every
other synthetic submission in `venustest` has always passed.

**Stated at the strength the evidence supports.** It is *not* "a ring the guest never
created": ring 0 is the CPU ring, needs no creation, and Mesa's first submit of every
renderer lifetime carries `RING_IDX` on it and completes — a `GPU3D_DEBUG` `(size, flags,
ring)` histogram over one boot shows both that first submit (`size=0x8C flags=0x04 ring=0`)
and the `vn_ring_destroy` teardown submit (`size=0x10 flags=0x06 ring=0`) completing on a
context where no host ring has ever been created. The variable is the **stream**:
`venustest`'s 32 zero bytes are not dispatchable (`vkr: vn_dispatch_command failed`), and
with `RING_IDX` the completion fence routes through the renderer context instead of the
global timeline, so a context whose dispatch failed never retires it. A genuinely
**nonexistent** ring index was not tested — our driver bounds-checks `ring_idx` against the
context's `num_rings` before it could get that far. Also **not separated, and not claimed
either way**: whether the non-retiring fence is a property of *that* submission's failed
dispatch or of a context already poisoned by an earlier one; `venustest`'s failing case
always ran on a context that had already had a stream rejected. Both readings give the same
answer to the question that mattered.

**Real Mesa cannot reach this** — its streams are valid Venus protocol, and `vktest`,
`vkrender` and `vkswap` issue dozens of `RING_IDX` submits per boot with zero timeouts. So
this blocks nothing. It is recorded because it is a denial-of-service shape available to
any future client that submits a malformed stream with `RING_IDX` — which is every Mesa
submission — and because a caller cannot tell it apart from a dead host. Whether the right
answer is a guest-side precondition, a shorter timeout with a distinct error, or nothing
at all is **undecided; this is a finding, not a plan.** Do **not** "fix" it by refusing or
rewriting `RING_IDX` kernel-side: Mesa sets it on every submit.

### 2. The last unproven hop is a Vulkan-free present on the non-Venus path

Everything up to the scanout is proven. `vkrender --present` scores 10/10 `present_*`
subtests with `failures = 0` and needed **zero code** — it was unrun, not unfinished — and
the QEMU wire trace shows the complete device-level handover: `RESOURCE_CREATE_2D res 0xb,
1920x1080` → `RESOURCE_ATTACH_BACKING` → **`SET_SCANOUT id 0, res 0xb`** →
`TRANSFER_TO_HOST_2D` → full-frame `RESOURCE_FLUSH`, with the console driver reclaiming
scanout 0 (`res 0x1`) when `vkrender` exits — a second, independent confirmation that the
scanout really had been handed over. `vkswap` separately drives a real swapchain to
`vkQueuePresentKHR -> VK_SUCCESS`, **21 PASS / 0 FAIL**, including a genuine
`UNDEFINED → PRESENT_SRC_KHR` barrier submitted on the queue and fence-waited, because
presenting an `UNDEFINED` image is undefined behaviour and a present that skipped it would
be a spec violation returning `VK_SUCCESS`. Attribution there is the cleanest in the whole
Vulkan arc: the same binary on a kernel with the PRIME commit reverted gives 16/1, and the
single failure is `create_swapchain` (`VkResult(-10)`).

**The photograph now exists, on both arches, and the present half of this item is
CLOSED** (`9d73b43`). The Venus host still cannot take one, and that diagnosis was correct
and is unchanged: bare `screendump` there returns a valid PPM of the *text console*, and
the `device=` route fails twice over — first `DeviceNotFound`, because QMP resolves
`device=` as a **qdev id** and `--venus`'s device line carried no `id=`; then, with
`,id=venusgpu` added, it works *before* the present (capturing Limine's stale 1280x800 boot
surface) and fails `"no surface"` only *after* the guest sets a scanout, because a
virgl-backed scanout is a GL scanout with no `DisplaySurface`. Host-tooling limit, not a
LeandrOS defect. The answer was to stop trying to photograph the Venus host at all.

**What closed it was `drmsmoke --hold`, not a new tool.** The item called for "a standalone,
Vulkan-free dumb-buffer present tool"; `drmsmoke` already walked the entire path
(GETRESOURCES → GETCONNECTOR → CREATE_DUMB → MAP_DUMB → mmap → ADDFB2 → SETCRTC → DIRTYFB).
Only two things were actually missing, and both were about the *photograph*, not the
present: it painted a gradient (awkward to verify) and it tore down and exited, after which
the console driver reclaimed the scanout. `--hold` paints `0x181818` with a 256x256
`0xFF0000` block at (64,64) and never exits. **A whole new tool would have been wasted
work** — worth remembering the next time this file sizes a task, because the item was
written as if from scratch.

**Result, on the default non-Venus path where the GPU has a real `DisplaySurface`.**
aarch64 1280x800: exactly **65,536** `0xff0000` pixels and **958,464** `0x181818`,
accounting for all 1,024,000. x86_64 1920x1080: **65,536** and **2,008,064**, all
2,073,600. Block corners exact at (64,64)–(319,319); the pixel just outside each edge is
field. **No third colour exists anywhere in either frame** — the text console is entirely
gone, which is the part that shows the guest owns the scanout rather than merely having
drawn somewhere. `SETCRTC: PASS` is cross-footed against the sentinel, because `--hold`
prints its sentinel even when painting was skipped; the sentinel alone is not proof.

So "does the DRM present path put pixels on a scanout" is now answered **by photograph**,
not only by wire trace, and it is separated cleanly from "does this Venus host have a
photographable display" (still no). Two notes for whoever runs this next. `--hold`
deliberately **diverges** rather than falling through — the default path's PRIME round-trip
overwrites pixel (0,0) and would corrupt the image being measured. And on x86_64 a
backgrounded `drmsmoke --hold &` produced **no serial output at all** while the identical
invocation on aarch64 did, with foreground on x86_64 passing every subtest; run it in the
foreground there and capture via the QMP monitor, which is a separate channel. That is an
output-routing difference in shell job control — **mechanism not established, recorded as an
observation only.**

**M4 goes via `MESA_VK_WSI_DEBUG=sw,noshm` — not bare `sw`, and not cross-open dmabuf.**
cosmic-comp does not advertise `zwp_linux_dmabuf_v1` here (Standing context; item 3), and
Mesa's WSI binds `wl_shm` *only* in the `sw` case and `zwp_linux_dmabuf_v1` *only* in the
non-`sw` case — mutually exclusive (`wsi_common_wayland.c:1406-1421`), so a non-`sw` Venus
on this compositor returns `VK_ERROR_SURFACE_LOST_KHR`. No Mesa rebuild is needed: the
shipped `venus-lane/stage-aarch64/usr/lib/libvulkan_virtio.so` was built
`-Dplatforms=wayland -Dvulkan-drivers=virtio`, contains the `MESA_VK_WSI_DEBUG` string and
its flag table, and has **both** WSI branches compiled in (`wsi_wl` ×30, `wl_shm` ×4,
`wl_shm_pool` ×2, `zwp_linux_dmabuf` ×8).

**Two corrections to this plan, both measured, both of which would have cost a session.**

1. **Bare `sw` does not work; it needs `sw,noshm`.** Measured on the Linux box's *host*
   (RADV, Mesa 26.1.4, KWin): `MESA_VK_WSI_DEBUG=sw` alone fails `vkCreateSwapchainKHR`
   with `VK_ERROR_INVALID_EXTERNAL_HANDLE`. Root cause read out of Mesa 25.3.6
   `wsi_common_wayland.c:3548-3556` — the sw path has **two** variants. Plain `sw` selects
   `WSI_WL_BUFFER_GPU_SHM`, which imports the `wl_shm` mapping as *device memory* and so
   requires `VK_EXT_external_memory_host` to be importable for the image's memory types.
   Adding `noshm` selects `WSI_WL_BUFFER_SHM_MEMCPY` — the same `wl_shm` buffer, filled by
   a CPU copy, requiring nothing of the driver. With `sw,noshm`: **5/5
   `vkQueuePresentKHR → VK_SUCCESS`**.
2. **`vkswap` is not a Wayland client and never was.** It, `vkrender` and `vktest` are all
   `VK_EXT_headless_surface` — none of them touches Wayland. **No Vulkan Wayland client
   existed in this project**, so the plan above named a vehicle that could not carry it.
   One was written (`vkwl`, built against the same musl toolchain that produced `wlclient`,
   so its `DT_NEEDED` closure — `libc.so`, `libwayland-client.so.0` — was already staged).
   It is validated **on the host first**, presenting 5/5 on the sw path *and* 5/5 on the
   dmabuf path, which is the right bisection: a guest-side failure can no longer be blamed
   on the client.

The route remains all userspace and zero kernel risk, and is still the correct bisection
point if the dmabuf route is ever attempted — it proves the client, the protocol, the
compositor wiring and the Vulkan rendering independently, leaving the kernel as the only new
variable. **Guest-side result against cosmic-comp is still outstanding**, as is aarch64; the
numbers above are host-side.

### 3. Cross-open dmabuf import — dead as an M4 route, alive for other reasons

`open_may_reach` (`drivers/src/drm_device_interface.rs:1237`) deliberately scopes BOs to
their owning DRM open, which is correct for `b80ab5a`'s ownership model but blocks
`VK_KHR_display` and Wayland dmabuf, both of which import into a different open (and, for
Wayland, a different process).

**Stages 3–5 of the design are killed as an M4 unblocker, by measurement.** cosmic-comp
advertises no `zwp_linux_dmabuf_v1` on a software renderer here (Standing context, with
the scope caveat), so no amount of kernel work reaches a Wayland Vulkan client in this
configuration — the missing global is upstream of the kernel entirely. M4 goes via
`MESA_VK_WSI_DEBUG=sw` (item 2). Stages 1–2 were never about M4 and have **landed**
(`3dbba0c`, `49399f9`), verified by mutation on both arches.

What remains worth doing here, once M4 is off it, and none of it is scheduled:

- **Venus importing a foreign dmabuf** — Vulkan-to-Vulkan buffer sharing between two guest
  processes (`vn_device_memory.c:110-124`, `vn_get_memory_dma_buf_properties`). This is
  the one consumer whose value does not depend on the compositor being accelerated, and it
  is fully served by Stages 1–3 without Stage 4. Note Mesa's importer **refuses** a BO
  whose `info.blob_mem` differs from its own (`vn_renderer_virtgpu.c:1181-1184`), so
  `RESOURCE_INFO` through an imported handle must report the *original* `blob_mem` — an
  assertion, not an obvious invariant.
- **A zero-copy client→compositor path** instead of §6.2's per-frame `memcpy` — real, but
  worth nothing while the compositor is softpipe and reads every pixel with the CPU.
- **`VK_KHR_display`**, which needs the whole deferred list on top (`SET_SCANOUT_BLOB`,
  absent; `MAP_DUMB`/`ADDFB2` accepting blob handles; the connector's missing `DPMS`) and
  is not a committed milestone.

**Sizing note, corrected.** If Stage 3 ever lands and imports start minting handles, the
resulting leak is small: cosmic-comp exports **per buffer at allocation, not per composited
frame** — 5 dumb creates and 5 PRIME exports over 185 s of live session, then frozen — so
the projection is ~5 objects per session, not thousands. `gem_handle_delete` already routes
`MODE_DESTROY_DUMB` and `GEM_CLOSE` through one path (`65fb20c`), so the retirement half of
that work is done; the `bo_dumb`/`bo_bhnd` census fields are the detector.

Design, staging and per-stage guard tests with their falsifying mutations:
`~/code/leandros-artifacts/notes/m9-crossopen-dmabuf/crossopen_design.md`.

### 4. Deferred work and known limitations

- **No shebang or binfmt support.** `execve` on a script fails with `Exec format error`, so
  `start-cosmic-leandros` — a shell script — must be launched as
  `brush /bin/start-cosmic-leandros`. Nothing depends on fixing this today, but every
  future "why does exec fail on that file" starts here.
- **Doom does not link relibc.** `../doomgeneric/Makefile.leandros` links
  `userland/target/<arch>-unknown-none/release/libleandros_libc.a`, whose allocator is
  `userland/libc/src/mem.rs` — a ~20-line **bump allocator over `brk(2)`** with no free
  list, no dlmalloc and no `mmap` path. The retired malloc-hang item had blamed relibc; it
  could never have been right. Worth stating plainly so the next person debugging a Doom
  allocation does not start there. doomgeneric's zone default is 4 MiB (`DEFAULT_RAM 4`);
  the 16 MiB case is reachable only via `-mb 16` and also passes.
- **Mesa modifier support.** The claim that our GBM lacking `gbm_bo_create_with_modifiers2`
  makes smithay reallocate the swapchain per frame is **refuted**:
  `allocator/swapchain.rs:154-181` caches slots and allocates only when `buffer.is_none()`,
  `allocator/gbm.rs:200-238` has a documented Invalid/Linear fallback, and an allocation
  failure would surface as `FrameError::Allocator` — no flip at all, not a degraded one.
  The idle damage counters confirm it from data. The separately-observed 128-dmabuf-fd burn
  in ~1 s and the `MAX_FDS` 64→128 raise are untouched by this and still stand.
- **llvmpipe** — the TCG-performance lever, staged but not landed. softpipe was chosen for
  correctness (portable C, no per-arch LLVM codegen bring-up ×2).
- **Synthetic sysfs** — the read-only `/sys/dev/char`, `/sys/class/drm`, `/sys/class/input`
  design in `docs/design/k4-drm-design.md` is execution-ready but deferred; no current
  consumer needs the enumeration.
- **DRM ioctl gaps cosmic-comp tolerates** (kernel returns Unsupported): `VRR_ENABLED`
  property, syncobj. Nothing optional is advertised in the property table on purpose —
  smithay guards each and degrades cleanly.
- **`FENCE_FD_IN`** (sync-file import) still needs the reverse plumbing and has no
  signalled-by-construction shortcut, unlike `FENCE_FD_OUT` (`09def61`). Real
  `DRM_IOCTL_SYNCOBJ_*` are not on the critical path — Mesa 25.3.6 compiles the SIMULATE
  path unconditionally. **A dependency to remember:** the out-fence eventfd is signalled at
  creation, which is correct **only while `VirtioGpu::submit` is a synchronous busy-spin**.
  If the ISR work ever makes submission asynchronous, that becomes a lie and must become a
  real waitable fence. The dependency is on `submit`, not on the syncobj code, and the
  source comment says so.
- **ELF loader follow-ups from the dynamic-linking wave**: interp is eagerly loaded
  (~4.8 MB per exec), and there is a pre-existing buddy-slack leak on the eager→lazy split.
- **`/proc/self/exe` returns `/bin/init`** regardless of the caller.
- **libseat shim eventfd workaround** (`0bed5ad`) is inert now that the kernel honours
  `EFD_NONBLOCK`, and can be simplified.
- ~~**DRM page-flip event timestamps**~~ — **done** (`b8ff2f6`), verified by mutation.
  `queue_flip_event` now stamps from `arch_monotonic_ns()`. The layering problem is worth
  remembering: `drivers` has **no Cargo edge** to the arch crates, and the tree's existing
  answer is a `#[no_mangle] extern "C"` symbol resolved at link time (`servers/evdev`
  already does this for input timestamps) — reach for that before adding a dependency edge.
  `drmsmoke`'s new `FLIP_TS_SUBTICK` is the permanent detector.
- ~~**Harness gotchas in `m8_cursor.py`**~~ — **both fixed** (artifacts repo, not under git).
  **Correction: the recorded mechanism for the first one was stale.** The file already keyed
  its busiest window on `evpush`; the surviving bug was that the no-activity branch fell
  through to `return 0` instead of erroring, so a legacy-path control still printed a
  degenerate `1.00 flips/s` — symptom recorded correctly, cause not. It now exits non-zero
  with a message. The positional-regex shear was real and is replaced by the
  order-independent `key=0xHEX` parser from `m9-fb-damage-clips/m9_analyze.py`, verified
  against a real capture carrying the `dmg_*` insertion **and** against a synthetic line
  with a field inserted mid-stream, so it tolerates the next insertion too.
- **Build gotcha:** building a userland test binary with a bare `cargo build` instead of
  `scripts/build-userland.sh` omits `-C relocation-model=static`, producing a PIE whose
  `.data.rel.ro` our loader never relocates. It then faults at `__libc_start_main+0x44`
  with `CR2=0`, before `main` — a distinctive signature whose cause is not obvious from the
  fault alone. Always build userland through `scripts/build-userland.sh`.

---

## Housekeeping

- **Fresh-worktree gotcha: the guest boots with no shell.** `build-all.sh` and
  `mkfs-f2fs-populated.py` resolve the sibling repos as `$ROOT_DIR/../<repo>`, and an agent
  worktree's parent is `.claude/worktrees/`, not `~/code/`. The build **exits 0** and only
  prints `⚠️ brush source not found … skipping`; the failure appears at runtime as
  `login: exec failed` / `session ended, restarting login`, i.e. `/bin/login` execve()ing a
  shell that is not in the image. `brush`, `coreutils` and `bottom-leandros` symlinks were
  added under `.claude/worktrees/` alongside the pre-existing `doomgeneric`, `mame` and
  `relibc`, and are left there deliberately.
- **`/bin/wl-globals` is staged when the host binary exists** —
  `~/code/leandros-artifacts/m9-wlglobals/out/wl-globals-<arch>`, same conditional pattern
  as `leandros-applet`. It is a measurement instrument (it enumerates the `wl_registry` of
  every `wayland-*` socket in `$XDG_RUNTIME_DIR` and exits); nothing in the session depends
  on it. **Both arches are now built and staged**; the crate's `.cargo/config.toml` already
  carried the x86_64 target section, it had simply never been exercised. Needs
  `cargo +nightly` — the default stable toolchain has no Linux musl targets installed.
  **Correction worth keeping:** the `-C relocation-model=static` landmine below does **not**
  apply to this binary. `wl-globals`, like `leandros-applet`, is a genuine *dynamically
  linked* PIE with a real `PT_INTERP` (`/lib/ld-musl-<arch>.so.1`), built with
  `-C target-feature=-crt-static -C relocation-model=pic` against `m3-gl-stack/sysroot-<arch>`.
  The landmine is about Rust's self-relocating *static*-PIE, which is a different recipe.
  Applying it here would break a working build. Both arches verified to have identical ELF
  shape (DYN, 11 program headers, same order).
- **Two spent instruments, kept but not pending.**
  `~/code/leandros-artifacts/notes/m9-damage-rootcause/damage_rect_dump.patch` (132 lines,
  one file, entirely inside the `DRM_STATS` gate) was applied on the box, produced the
  damage answer, and was reverted; it still applies clean and remains a usable clip-list
  dumper, but the question it was built for is closed, so it is not prepared work.
  `m9-dmabuf-lifetime/dmabuf_lifetime.patch` landed as `49399f9` — do **not** re-apply it;
  it is kept only because its companion `dmabuf_lifetime.md` is the reference for the
  refcount model.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at exit
  144. Prefer `scripts/scmrun.py` (one process per command, one held serial connection,
  explicit pre-send drain, fixed read window, no `expect()`) over `driver.py cmd` for
  anything whose number will be quoted, and open every boot with a positive control.
- When host tracing is on, always pass `-D <file>`. A trace stream sharing the guest's pty
  interleaves per character and silently destroys both `grep` results and harness sentinels
  (instrument-reliability entry 12).
