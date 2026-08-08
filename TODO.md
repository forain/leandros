# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-07**, after a wave that landed eleven commits,
**closed the present half of item 2 with an actual photograph**, **achieved M4's client
half**, emptied "Prepared but not landed", cleared four deferred-list entries, and corrected
**seven** recorded claims — two of which this file had itself introduced hours earlier.

The whole artifacts tree was also imported into `artifacts/` in this repo, because it had
been carrying the entire Vulkan arc's instruments with no version control at all.

**Two lessons from this wave that generalise, both about sizing rather than engineering.**

1. **Two of the four "open" items were never work.** Item 1 is a finding whose own text says
   the fix is undecided and warns against implementing one; item 3 is dead by measurement
   with nothing scheduled. A four-row table implies four tasks and this one held two.
   **Read what an item says it is before scheduling it.**
2. **Item 2 was over-sized.** It asked for "a standalone, Vulkan-free dumb-buffer present
   tool". No new tool was needed — `drmsmoke` already walked the whole path, and the two
   genuinely missing pieces were about the *photograph* (a checkable pattern, and not tearing
   down before capture), not the present. ~85 lines instead of a new binary. **Check whether
   an existing test binary already covers the path before sizing from scratch.**

**A third, sharper lesson: a control on the wrong machine is not a prediction.** The
`MESA_VK_WSI_DEBUG=sw,noshm` requirement was measured correctly on the RADV host and recorded
here as though it held generally. The guest measurement **reversed it** — Venus reports
`VK_EXT_external_memory_host = no`, so Mesa already selects the memcpy path and plain `sw` is
correct. A host control is a *client sanity check*, not a guest prediction. The same shape
recurred with the interpolated clock: a clamped `tv_usec` was recorded as "no signal" when it
is in fact **positive evidence**, because the old clock could never emit a value ending in
`9999`. Both were caught within the wave; both would have misled the next reader.

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
   measured in `artifacts/notes/m9-dmabuf-lifetime/mac-verify.md` §5.3-5.5:
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
6. **Zero TODO-citation violations existed, not three and not one.**
   `userland/vfstest/src/main.rs:1` and `userland/f2fstest/src/main.rs:1` had already been
   fixed by `033f3d0`, an ancestor of HEAD. The `driverpy_venus.patch` citations were real
   and are now removed. The "one new violation" recorded here — `73258ea`'s `bo_dumb`
   doc comment citing "item 9" and repeating the refuted per-frame severity claim — **was
   already fixed by `20525aa`**, also an ancestor of HEAD, which dropped the citation *and*
   corrected the magnitude to per-exported-buffer in the same commit. `git grep` for
   `TODO\.md item\|TODO item [0-9]` over `drivers/ servers/ userland/ kernel/ mm/ arch/
   scripts/` at HEAD returns **nothing**; the only hits anywhere are dated reports under
   `artifacts/notes/`, which are historical records and out of the rule's scope.
   **The sharper lesson, and it is about this file rather than about the code: the entry
   claimed it was "found by `grep …` at reconciliation time", and that grep cannot have
   been run — the pre-`20525aa` text would have matched it and the post-`20525aa` text does
   not.** A recorded provenance is not evidence that the command was run. This is now the
   *second* citation entry in one wave that reported a violation an ancestor had already
   cleared, and both would have been caught by actually running the one-line grep against
   HEAD. Run it; do not narrate it.
7. **The two trees were never divergent as recorded.** The box was already at `a1568ec`
   when this wave began, not at `a0325c6`, with `git patch-id --stable` matching on both
   machines; it has since been synced forward. The standing lesson survives the correction
   and is the reason the divergence was misread in the first place: **compare by
   `patch-id`, not by SHA.** These two trees have twice received the same change under
   different SHAs.

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment **totally unmodified** (source:
`../cosmic-epoch`) on both x86_64 and aarch64 under QEMU; build-configuration flags
(`--no-default-features`) are allowed.

**The policy, stated exactly, because the previous wording was an absolute that the tree
refuted.** *Temporary* edits to COSMIC source **are allowed, for investigation and discovery
only** — prints, counters, panics, anything that localises a fault. They **must be reverted**
before the work lands, and the revert must be **proven, not asserted**:
`git -C ../cosmic-epoch status --porcelain` empty, submodules likewise (pinned at
`epoch-1.3.0`). A reverted tree with an instrumented binary still staged in the image is worse
than either, so rebuild from clean source before shipping. **Record what the instrumentation
taught you in `artifacts/notes/`** — a finding whose only evidence was a deleted `eprintln!`
is a finding nobody can re-check. **Any permanent fix belongs on the LeandrOS side** — kernel,
shims, or our own launcher — never in COSMIC.

**Two PERMANENT patches currently violate this and need resolving**, rather than being quietly
tolerated: `ports/cosmic-session/0001-env_rx-timeout-fallback.patch` and
`ports/cosmic-greeter/0001-locker-idle-without-logind.patch`. (`ports/busd/current-thread-runtime.patch`
is fine — busd is ours.) The session one **fires every boot** — `handshake did not complete`,
so every session runs on a 5 s fallback and no child receives cosmic-comp's exported
environment — and its recorded root cause, "a tokio-integration residual", is **asserted, not
demonstrated**. Under this policy that patch is a standing debt with an unproven
justification: root-cause it and fix it on our side, or demonstrate why it cannot be. Everything beneath COSMIC — kernel, libc,
system libraries, daemons — is ours. **This constraint stays load-bearing**: the missing
dmabuf global is behind cosmic-comp's `!is_software` gate (item 3), and the reachable
outcome there is a measurement, not a patch. The one place it looked like it would force an
upstream bug report — the primary-plane damage — turned out to have no upstream bug at all.

**Where it stands.** The desktop runs on both arches: cosmic-session → cosmic-comp on
KMS/softpipe → busd → cosmic-bg + cosmic-panel renders a wallpaper plus a full-width
panel bar with an embedded Wayland client, clock ticking. **"Remaining desktop work is
quality and performance, not bring-up" was wrong** and is corrected by the survey in *Road to
a complete COSMIC desktop* below: **no input of any kind reaches the compositor**, and no
libcosmic/iced application renders at all. Both are bring-up, not polish. Vulkan runs **and presents**: `vkrender` executes
fill-buffer, compute and graphics work, `vkswap` drives a headless-surface swapchain to
`vkQueuePresentKHR -> VK_SUCCESS`, and `vkrender --present` puts a rendered image on a
real DRM scanout.

**Suite baselines.** On fresh images with `vfstest` run exactly once per image, both
arches: vfstest **36/0**, scmtest **32/0**, drmsmoke **29/0** (22 → 25 with `edad115`'s console guards, → 29 with `c8cbbc1`'s atomic lane), wakepolltest 10/0,
forktest 3/0, epolltest **10/0** (was 9/0 — `proc_pid_exe` added with the
`/proc/<pid>/exe` fix), polltest 6/0, sigtest 6/0, timertest 6/0, memtest 4/0,
idletest 2/0 (`IDLE_CPU_US 0`), evtest2 8/0. `waittest` has **4** subtests and is **4/0 or 3/1 on either arch** (a harness reporting 5/0 is miscounting the summary line — see instrument entry 11) — a pure timing race in `fork` → child `setpgid(0,0)`+`_exit` →
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
`artifacts/notes/m9-crossopen-dmabuf/stage0a-wl-globals.md`.

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
`artifacts/m6-session-data/start-cosmic-leandros`):

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
  the defect or the commit instead; those do not move. **No violation is outstanding.**
  `git grep -n 'TODO\.md item\|TODO item [0-9]'` over the source trees returns nothing at
  HEAD; hits under `artifacts/notes/` are dated reports, which are historical records and
  deliberately out of scope. (`userland/vfstest/src/main.rs:51` names an item by *topic*,
  "extended attributes", not by number, and is fine — the rule is about numbers, which
  move.) **Re-run that grep at reconciliation time and paste what it returns.** Twice now
  this file has recorded a violation that an ancestor of HEAD had already fixed, the second
  time while asserting the grep had been run.

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

**Instrument reliability — read this before trusting a number.** **Twenty-three** separate
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
   (`if git apply --check …; then`). **Hit a second time, live, while reconciling this very
   file:** `pgrep -fl 'build-all.sh' | head -5 || echo "(no build running)"` printed
   *neither* a process list nor the fallback, because `head` exited 0 and swallowed `pgrep`'s
   "no match" status. The `||` branch is just as status-blind as the `&&` branch. Use
   `if pgrep -f …; then … else … fi`. Two independent instances now; this is the most
   frequently re-walked trap in the list.
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
20. **A truncated build log is uninformative in BOTH directions, and it misled in the
    unexpected one.** A delegated lane's `tee`-redirected copy of `build-all.sh` desynced and
    stopped mid-run at `Output: mame-aarch64 (372M)` — 22 KB, **zero** `error`/`failed` lines.
    `grep -ciE '^error|error\[|failed'` returns **0**, *identical* to a clean build and the
    same shape as entry 4's `grep -c ': FAIL'` on a truncated capture. That is the expected
    trap. **What actually happened was the mirror image: the build had SUCCEEDED, and the
    truncated log plus an absent process led the coordinator to record it as killed.** The
    lesson is therefore stronger than "a truncated log can fake success" — a truncated log
    supports *whatever* you already suspect, and "no process running" is equally consistent
    with finished and with dead.
    **Cross-foot against the build's products, not its log.** One `ls` settled it:
    `f2fs-data0-aarch64.img` was written at **09:38:05**, two minutes *after* the log's last
    line at 09:36. Products have timestamps; a log only has content, and content is what
    truncation destroys. Corollary for delegation: require the success sentinel
    (`🎉 Build Complete!`) *or* a product mtime — never infer either outcome from silence.

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
    **It recurred on 2026-08-07 in a harness written that same day** (`m11_atomic_console.py`),
    which reported `waittest 5/0` on both arches. The committed log shows exactly four
    subtests — `wnohang_poll_until_exit`, `blocking_wait_for_exit`, `echild_no_children`,
    `wait_on_process_group`, all PASS — plus the summary line, and `grep -c ': PASS'` on it
    returns 5. The true result was a clean **4/0**. **Documenting a trap does not stop the next
    harness walking into it**, because each new harness re-implements the extractor from
    scratch. The durable fix is to stop counting lines and read the binary's own
    `failures = N` trailer, which every one of these test binaries prints and which cannot be
    inflated by a summary line.
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
23. **A single sample cannot tell "the pixels never arrive" from "the pixels had not arrived
    yet" — and it produced a confident, geometrically perfect FALSE FAILURE.** The aarch64 M4
    run 1 sampled each hold once, at +6 s, and **failed** phase B — having caught a solid
    rectangle of `vkwl`'s *previous* frame colour at **exactly** the right geometry: 151,868 px,
    478x318, fill 0.9991. Every structural check passed; only the colour was a frame stale, so
    the failure looked like a real one. The cause is a sentinel meaning less than it appears:
    **`HOLD READY seq=N` means the client returned from `vkQueuePresentKHR`, not that the
    compositor composited and flipped.** The client bursts its frames in seconds while the
    compositor is still draining. Fixed by sampling a *series* rather than a point, which
    converts the ambiguity into a number: in run 2 the window is **absent from the screen
    entirely** at +6 s and correct at +26 s. Run 1 is committed unedited, because a
    well-formed false failure is worth more in the record than a quiet re-run.
    **Open, ~1 line:** that harness's `HOLD_RE` can match a line only *partly* arrived over the
    8-byte-at-a-time serial link — it matched `secs=1` in run 2 phase C and zeroed that hold's
    sampling budget. It passed on sample #1 anyway, but it would otherwise have scored a hold
    it never sampled. Anchor the regex on a line end.
22. **`git add` skips ignored paths silently, so a commit can look complete while dropping
    exactly the evidence it exists to preserve.** `artifacts/.gitignore` excluded `*.png` and
    `*.log` to keep build output out and swept up every capture with it. `dc013c0` committed
    the aarch64 vkrender `results.md` and `precommit-pass-criteria.txt`; the three frames and
    both logs were dropped **without an error, without a warning, and with a clean
    `git status` afterwards** — the lane reported them as landed in good faith. Caught only
    because the next step was to re-census the PNGs and they were not there. Fixed by
    `!notes/**/*.png` / `!notes/**/*.log` (`03ee4ee`). **The general shape: an exclusion rule
    written for one category will silently capture anything that shares its file extension,
    and `git add <dir>` reports nothing.** After committing evidence, `git show --stat` and
    count the files. Every other entry in this list is about a measurement that lied; this one
    is about the measurement surviving at all.
21. **An in-guest polling loop wedged the guest shell outright.** Relaying a test binary's
    sentinels by redirecting its output to a file and polling with `$(grep … | tail …)` in a
    loop produced **20 minutes of total console silence** — no sentinel, no `M4: DONE`, not
    even the `cat` that followed the loop. Not root-caused; the working shape was to run the
    binary in the foreground and **cut the output at the source** (`vkwl` gained a `quiet`
    mode) rather than filter it downstream. Recorded as a landmine, not a diagnosis: if you
    need a long guest run to be quiet, make the *program* quiet.
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
`artifacts/notes/m9-m3-vulkan/build-vkrender-alpine-fixed.sh`,
`build-vkrender-aarch64-zig.sh`, and `m9-vkswap/build-vkswap-alpine.sh`. The Vulkan loader
stays unshipped: the ICD exports only `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr`, so it can
never stand in for `libvulkan.so.1`. `vkrender`'s `s2_checksum` is **printed but not
asserted** unless `VKRENDER_EXPECT_CHECKSUM=0x02C0FDC5` is exported; every comparison so far
has been done by hand.

**Evidence lives outside this repo.** Run logs, screenshots, research notes and test
harnesses are in `artifacts/notes/`. Design docs that are still
execution-ready are in `docs/design/`.

**Explicitly out of scope** (all degrade gracefully or are non-fatal): XWayland,
PipeWire/audio for COSMIC, NetworkManager, UPower, accountsservice, greetd +
cosmic-greeter, cosmic-workspaces' wgpu path, hotplug, VT switching, multi-seat.

---

## Open work

| # | Item | Category | State |
|---|---|---|---|
| 6 | Input **does** reach clients; the virtqueue starves under load | Bug — **REFRAMED** | Delivery falls 57% by rate; cosmic-comp innocent |
| 7 | **Only a raw `wl_shm` client renders — no libcosmic/iced app draws** | Bug — **actionable** | `cosmic-settings` alive, healthy, 0 px in 74 s |
| 8 | Nothing to launch, and no way to ask for it | Feature — **config DONE** `52665aa` | Keybinding table staged; still no terminal to launch |
| 9 | **9 panel applets have no scoped-out dependency and are simply unbuilt** | Feature — cheap | Build recipe exists; spawn path proven |
| 10 | **busd has no D-Bus activation**, so no portal, no screenshot, no file chooser | Feature — structural | `<servicedir>` deliberately omitted |
| 11 | **Two PERMANENT COSMIC source patches** — the goal is totally unmodified | Debt — **actionable** | `env_rx` fires every boot; cause asserted, not shown |
| 1 | A host-refused `RING_IDX` submit costs a full control-queue timeout | Finding — **no action** | Recorded on purpose; fix undecided |
| 2 | M4 — **COMPLETE, photographed on BOTH arches** | Feature — **DONE** | `132d4df` x86_64, `d91edbf` aarch64 |
| 3 | Cross-open dmabuf import — dead as an M4 route, alive for other reasons | Feature — deferred | Nothing scheduled |
| 4 | fb console scrolled the scanout out from under cosmic-comp | Bug — **FIXED + GUARDED** | `edad115` fix, `c8cbbc1` closes the guard gap |
| 5 | Deferred work and known limitations | Mixed | Backlog |

---

## Road to a complete COSMIC desktop

Written 2026-08-07 from **two evidence-gathering lanes, not from a wishlist**: a source
inventory crossing `cosmic-session`'s spawn list against what `mkfs` actually stages, and a
live capability probe on x86_64/KVM over two runs and 13 minutes. Everything below is
measured or read from source; where a cause is unknown it says so.

**Where the desktop genuinely is.** Better than the old summary implied. It composites; the
panel renders and its clock ticks; **a client opens a toplevel** (208,563 px at
`x=685..1233 y=133..521`); **two toplevels cascade, stack, and get a 1 px focus ring on
exactly one**; the compositor advertises **54 globals** including `wl_seat` **v9**,
`xdg_wm_base` v7, `zwlr_layer_shell_v1` v5, `ext_session_lock`, `ext_workspace_manager_v1`
and `zwp_virtual_keyboard_manager_v1`; and **all 12 COSMIC components plus busd stay alive
with zero crashes over 13 minutes**. `zwp_linux_dmabuf_v1`/`wl_drm` are the only notable
absences and are expected (`is_software`).

**The one thing that turns most of that into a desktop is input, and there is none.**

**Read the taxonomy before reading the list.** These four states look identical on a blank
screen and imply completely different work: **absent from the image**, **staged but never
launched**, **launched but crashes**, **runs but renders nothing**. This project has already
been misled once by conflating the last two — the panel was recorded as "frozen at its first
frame" and the measurement eventually exonerated both the kernel and cosmic-comp. Every row
below states which it is.

### 6. Input DOES reach clients — the item as filed was WRONG, and the real defect is throughput

**REFUTED and rewritten (`b22364d`, `ada294f`).** The original claim — "no input of any kind
reaches the compositor, not laggy, not partial: zero" — is false. A purpose-built Wayland
client, `/bin/wlinput` (`artifacts/m14-wlinput/`), mapped an `xdg_toplevel` against **stock,
unmodified** cosmic-comp and received everything:

```
ptr_enter=2  ptr_motion=31  ptr_button=3  ptr_frame=37  kbd_key=12  kbd_mods=3
keyboard.key 30/48/35/23/1/125, press+release each, in order
keyboard.modifiers depressed=64 (Super);  pointer.button b=272
```

Six keycodes, exactly the six injected, in order. Controls green *first* — `BOUND globals=54`,
`SEATCAP caps=0x7`, `CONFIGURE ×3`, `MAPPED 640x480` — so a zero would have meant something.
The capture shows the window on the wallpaper wearing cosmic-comp's **cyan focus border**.

**And delivery above evdev is not merely working but exact:** keyboard `evdev push +24` = 12
key events → client `kbd_key = 12`; pointer `push +68` = 17 motion frames → client
`ptr_motion = 17`. **Lossless, not "most".** libinput, smithay's drain, `process_input_event`,
seat lookup, abs→output mapping and focus routing are **all correct**. cosmic-comp is innocent.

**All three suspects eliminated.** (1) `input_devices: Disabled` — absent; both user and system
config trees dumped, nothing there, and independently impossible since `SEND_EVENTS_DISABLED`
closes the device fd while cosmic-comp demonstrably read `event1` all run (`rpid=37` on every
`[EVSTAT]`). (2) **libseat activation — the earlier benign reading was CORRECT**, and for a
better reason than the one recorded: `smithay/src/backend/session/libseat.rs:88-91` calls
`seat.dispatch(0)` then `rx.try_recv()` **specifically** to catch an enable delivered
synchronously from inside `libseat_open_seat` — our shim's contract is the exact case that code
was written for, so `active` is true from construction. Moreover **nothing on the input path
reads it**: `process_input_event` and the libinput calloop closure never call `is_active()`;
its only readers are KMS paths like `apply_config_for_outputs`, which early-return when
inactive and would have left every output unmodeset. The desktop renders, so it is active.
**No shim edit was needed or made.** (3) abs→output mapping — `pointer.enter x=319.9 y=378.0`,
inside a 640x480 toplevel, not clamped.

**What the original measurement actually saw.** The "0 px outside the clock rectangle" was
real, and the reasoning that made it readable was sound — the panel clock repaints once a
second, so a byte-identical capture proves a *stale frame* rather than a quiet desktop, and the
same route later showed 208,563 px when a client mapped. What was wrong was the **inference**:
"nothing changed on screen" was read as "no input arrived", when input was arriving and nothing
was *drawing a response* — no cursor is ever rendered (`curs_up = 0` per phase, one upload
ever, at startup), and the COSMIC apps that would have reacted are the ones item 7 shows render
nothing at all. **A null observation at the end of a long chain does not locate the break in it**,
and this cost two lanes. Instrument at the *destination* — a client that counts what it is
sent — before instrumenting the path.

**RESOLVED 2026-08-08. The handoff never starved — `arch::putc` did, and it took the timer
tick with it.** The ladder below reproduces byte-for-byte, and the decline in it is real, but
it is not buffer exhaustion under load: it is this harness measuring its own back-pressure.
Measured on an idle guest with **no compositor running**, x86_64/KVM (`artifacts/m14_rate.py`),
before and after the fix, with the harness **unmodified**:

| rate/s | moves | qmp_ok | qmp_rej | before ev/move | after ev/move |
|---|---|---|---|---|---|
| 2 | 20 | 40 | **0** | 2.00 | **4.00** |
| 10 | 100 | 200 | **0** | 1.12 | **4.00** |
| 30 | 300 | 600 | **0** | 0.91 | **4.00** |
| 60 | 600 | 1200 | **0** | 0.85 | **4.00** |

Host-side `virtio_input_queue_full` across the whole ladder: **1572 → 0**. 4.00 ev/move is
lossless — each move is two QMP commands and QEMU syncs per command, so a delivered move is
`ABS_X, SYN, ABS_Y, SYN`.

**What decided it — three phases in ONE boot at a fixed 60 moves/s, differing only in who is
draining QEMU's serial chardev** (`artifacts/m15_serial_stall.py`; loss counted host-side from
QEMU's own `virtio_input_queue_full` trace, so the instrument cannot be throttled by the stall
it is looking for):

| serial consumer | frames | queue_full | delivered |
|---|---|---|---|
| connected, never reads | 1200 | 1086 | **9.5%** |
| connected, reads throughout | 1200 | 0 | **100.0%** |
| not connected at all | 1200 | 0 | **100.0%** |

**The eventq delivers 100% at 60 moves/s — 240 events/s against 32 descriptors — whenever the
console is not back-pressured.** There is no load-dependent loss to explain.

**Mechanism.** `putc` polled the UART transmitter with **no bound** (`arch/x86_64/src/lib.rs`,
LSR bit 5; `arch/aarch64/src/uart.rs`, PL011 `FR.TXFF`), and QEMU's 16550 withholds `LSR.THRE`
for exactly as long as its chardev back end refuses the byte — `hw/char/serial.c:serial_xmit`
installs a `G_IO_OUT` watch on `EAGAIN` and returns *without* setting THRE. A socket chardev
with **no** client returns `len` and never blocks (the already-recorded “QEMU serial drops
output w/o client”); a client that is **connected and not reading** blocks. `putc` is reached
from **IRQ context** — the 0.5 Hz `[EVSTAT]` census runs off the timer tick via
`poll_deadline_tick` — so a parked reader wedged CPU 0 inside the timer IRQ handler:
`TICK_COUNT` froze, `sched::timer_tick_irq` never ran, and `poll_events()` never ran. Every
shape in the ladder falls out of that — ~2 s of live guest per rung, a constant **+32** flush
(exactly the ring) on unwedge, and a fraction that falls only because the denominator rises.
**The delivered COUNT per rung was flat; only the delivered FRACTION fell.**

**Fixed**: `putc` on both arches now waits against a **cycle-counter deadline**, not an
iteration count, and latches `TX_WEDGED` so a back-pressured console costs one probe per byte
instead of one deadline per byte. Console output may be lost; an interrupt handler may not be
stalled. An *iteration* bound is not enough and the intermediate measurement proves it: 10 000
LSR reads is ~10 ms against a real UART but ~100 ms against an emulated one (each `in al, dx`
is an exit to host userspace), and that version moved the parked case only from 9.5% to 27.3%.

**Both original suspects were wrong, and the counters say so.** A gated `[VQSTAT]` census
(`drivers/src/virtio_keyboard.rs`, `VQ_STATS`, committed **off**) recorded **`skips = 0` for
the entire run** — `try_lock` contention on `VIRTIO_INPUTS` cannot happen anyway, since
`poll_events()` is called only from the `cpu == 0` arm of `on_tick` on **both** arches — and
`maxb = 32` with `notify` advancing by exactly +1 per burst, which is a drain that stops and
restarts, not one falling behind. The missing volatile accesses and barriers **were** fixed
(`used.idx` read, `avail.idx` publish, matching `virtio_gpu.rs:189-191`) but were **not** the
cause: the x86_64 disassembly of the old code is faithful and correctly ordered under TSO
(`mov %cx,0x4(%r12,%rax,2)` immediately followed by `incw 0x2(%r12)`). They are a real latent
defect on **aarch64**, where nothing orders those two stores.

**Two things this item previously recorded as fact are false.** (a) “`drop+0` on every evdev
sample, which exonerates the ring” — `drop` reaches **680** in the very serial log that
sentence was written from. `MAX_EVENTS = 256` (`servers/evdev/src/lib.rs:44`), depth pins at
256 from the 30/s rung onward because nothing is reading the node, and it overwrites from
there. It does not change the ladder's arithmetic (`push` is counted before the ring is
consulted), but the ring was **saturated, not exonerated**. (b) “the loss is in the virtqueue
handoff, before `push_event`” — the handoff is lossless at every rate tested.

**Under a live COSMIC session the ~22×-worse figure (1,787 moves → 68 events) has not been
re-measured** and should be assumed to have had the same cause until it is: that harness also
held the serial socket.

**Found on the way out, x86_64-only and PRE-EXISTING: console output has no flow control at
all, and it is very likely why the x86_64 suite harness has never been trustworthy on the
box.** `drivers/src/serial.rs::write_byte` — the path userspace console writes take — is an
unconditional `out dx, al` into the 16550's transmit holding register with **no `LSR.THRE`
check**; the `#[cfg(not(x86_64))]` arm goes through `arch_serial_putc`, which waits, so aarch64
does not have this. Printing 300 numbered lines through a continuously draining reader returns
**19 of 300** — lines 0–18 and then nothing until the trailing marker — and the number is
**identical** on a kernel with the new `putc` deadline and on one with it removed, so it is not
this lane's doing. It explains the `m13_suite.py` shape on x86_64 exactly: vfstest's 36
subtests arrive 16 in its own window and 20 in the next, every later row then reads the
previous row's exit status, and **widening every budget to 700 s reproduces it identically**
because nothing is timing out — the bytes are never sent. The 2026-08-07 run recorded in
`artifacts/notes/m13-cosmic-config/` already shows the same shape. The fix is to make
`write_byte` wait the way `arch::putc` now does (safe now that `putc` carries a deadline), but
it gives every console byte an extra VM exit and needs its own suite run, so it is deliberately
**not** bundled with an input-path change. Measurement and md5s:
`artifacts/notes/m15-serial-stall-20260808/console-loss-preexisting.md`.

**The general lesson, and it has now cost three lanes: an instrument that has to print cannot
measure anything that printing can stall.** Prefer a host-side witness. Full write-up, the
`[VQSTAT]` series and raw logs: `artifacts/notes/m15-serial-stall-20260808/`.

**Still open on the render side, and now the more interesting half:** input arrives and nothing
draws. No cursor is ever composited. That is likely one investigation with item 7.

**Ruled out, with evidence — do not re-measure.** QEMU → virtio-tablet → kernel ring →
userspace read is **completely fine**: `evtest2` reads `/dev/input/event1` with no libinput in
the path and passes everything including `motion_abs_frame`, 32 events, monotonic timestamps.
The **libudev shim is not the break** — it enumerates from a *static table*
(`ports/input-stack/shims/libudev/libudev.c`) already containing `event0`
(`ID_INPUT_KEYBOARD`) and `event1` (`ID_INPUT_MOUSE`) with `/sys/class/input/inputN` parents,
so the non-listable `/dev/input` and absent `/sys/class/input` do **not** by themselves hide
the devices. **The libseat shim's open path looks correct** — `libseat_open_device`
(`ports/input-stack/shims/libseat/libseat.c:132`) opens `O_RDWR|O_NONBLOCK|O_CLOEXEC` and
returns the fd as the device id.

**The break is now bracketed much more tightly: everything below AND INCLUDING libinput is
exonerated by measurement** (`a4885ed`). libudev shim — both devices enumerated, `ID_SEAT →
seat0`, `ID_INPUT_KEYBOARD → 1`, and `dev_from_devnum type=c major=13 minor=64` **resolves**
(libinput drops a device *silently* if that round-trip fails, `evdev.c:2116-2147`). libseat
shim — `open_device /dev/input/event0 → fd=26 errno=0`, `event1 → fd=27 errno=0`. Kernel evdev
— `ioctls=21 enotty=0`, and under provocation `push+476 reads+234 eagain+117 deliv+476
drop+0 pollin+265`. calloop — **265 POLLINs against 234 reads**, and since `libinput_dispatch`
runs *only* from smithay's `process_events` (`smithay backend/libinput/mod.rs:714-722`), the
reads prove the source is being dispatched and nested epoll works. **And libinput itself
produces events**: `event1 - QEMU Virtio Tablet: is tagged by udev as: Mouse`, `device is a
pointer`, `DEVICE_ADDED` ×2 with correct caps, **`motion_abs=62 key=8 dispatch_err=0`**.
**libinput produces events and the compositor acts on none of them.** So the break is *above*
libinput's queue: smithay's `for event in &mut self.context` drain, or cosmic-comp's
`process_input_event` / seat routing / absolute-motion→output mapping.

**Two dead ends, both recorded so they are not repeated.** (a) **`wl_seat` capabilities are a
null result by construction** — measured `caps=0x7 pointer=1 keyboard=1 touch=1`, but
cosmic-comp calls `add_keyboard()`/`add_pointer()`/`add_touch()` **unconditionally** at seat
creation (`cosmic-comp/src/shell/seats.rs:190-243`), with an upstream comment explaining that
clients would otherwise race the compositor. `libinput_udev_assign_seat` returning `Ok` is
equally vacuous — it succeeds with zero devices. **This was my suggested discriminator and it
was wrong**; a capability that is always advertised cannot distinguish anything. (b) The log
route is dead **twice over**: `cosmic-comp/src/logger/mod.rs` pins `smithay=warn` in release
via `add_directive`, which `RUST_LOG` **cannot** override, *and* its `fmt::layer()` writes to
**stdout**, not stderr. So the discarded-stderr finding above is real but insufficient — fixing
it alone would still yield nothing.

**Three suspects remain, cheapest first.**
1. **`~/.config/cosmic/com.system76.CosmicComp/v1/input_devices` with `state: Disabled`**
   produces exactly this symptom. Nearly free to check.
2. **Our own libseat shim may never activate the session.** It fires `enable_seat`
   *synchronously from inside* `libseat_open_seat`, and `libseat_get_fd` returns an eventfd
   that **never signals**. If smithay's `LibSeatSession` never emits
   `SessionEvent::ActivateSession`, cosmic-comp may treat the session as inactive and discard
   input. **This is in our code.**
   **And it directly contradicts a conclusion recorded earlier the same day** in item 5's
   libseat entry, which reasoned that not reading `conn_fd` "remains correct independently of
   the kernel fix, because nothing ever writes that eventfd". *Nothing writing it* is precisely
   the suspicion now. That entry was sound about the `read()`; it did not ask whether the fd
   was supposed to be **written**. **A component can be correct as a consumer and broken as a
   producer, and checking only the half you touched will miss it.**
3. cosmic-comp's absolute-motion → output mapping — virtio-tablet is an *absolute* pointer.

**The decisive next test is neither of those**: run a Wayland client with a mapped
`xdg_toplevel` that binds `wl_pointer`/`wl_keyboard` and logs `enter`/`motion`/`key`. Receives
events ⇒ cosmic-comp's input path is fine and the failure is cursor/render-side. Receives
nothing ⇒ cosmic-comp drops them. That halves the remaining space before any suspect is
touched.

**Three further defects the census exposed, none of which is the cause.**
- **~91% of injected pointer motion is lost between QEMU and the guest ring**: 4632 QMP events
  accepted, **0 rejected**, → **412** evdev pushes (a session run: 3618 → 552). Even a fixed
  compositor would get a badly decimated pointer.
- **The in-kernel console steals keyboard events**: `dev=0 push=128 conspop=112 deliv=16`.
  `read_input_byte` pops from evdev device 0, and our evdev has **one ring per device**, not a
  per-open client queue as Linux has — so **two readers rob each other**.
- `servers/evdev/src/lib.rs` answers `EVIOCGVERSION` by returning the version as the syscall
  *value* instead of writing it to the user pointer. libevdev only checks `rc < 0`, so it is
  not fatal, but `driver_version` is left uninitialised.
- Unresolved and explicitly *not* concluded: `poll(2)` on a nested epoll fd reported readable
  1 time in 284 while `epoll(7)` on the same fd demonstrably works — confounded by the probe's
  own console-print stalls. Needs a clean two-level epoll test.

**The path is currently unobservable, and the reason is probably not the one first recorded.**
`RUST_LOG=smithay::backend::libinput=debug` is rejected because DEBUG is compiled out
(`release_max_level_info`, `cosmic-comp/Cargo.toml:61-62`, not raisable additively), and
**148 lines of session log mention libinput, a seat or an input device exactly zero times.**
**But cosmic-comp's stderr is DISCARDED**: `cosmic-session` spawns it through `launch_pad`
with stderr piped and registers **no `on_stderr` handler**
(`../cosmic-epoch/cosmic-session/src/comp.rs:122-134`). So the compositor's own diagnostics go
nowhere, and the only cosmic-comp output ever seen in a session log is what leaks out through
*other* processes — an error line expected from cosmic-comp was observed surfacing via
**cosmic-idle**. **The silence is very likely nobody reading the pipe, not smithay having
nothing to say**, which would mean the existing INFO-level logging is sufficient and no new
instrument is needed. Get at it *without* patching COSMIC: run `cosmic-comp` standalone with
stderr redirected, or redirect in `start-cosmic-leandros`, which is our own script.
**Two adjacent facts worth not rediscovering:** the guest has **no `grep`** (filter host-side
after pulling logs), and console writes repaint the framebuffer *which is the scanout*, so a
single capture can photograph console text — `edad115` gates this while a DRM master owns the
scanout, but sample a series and treat the first frame as suspect.
The cheapest datum that splits the space is **`wl_seat`'s advertised capabilities**, which no
shipped tool prints: `0` means libinput never found or was never given the devices (bug below
smithay's event loop); pointer+keyboard present means they were found and events are being
dropped or never dispatched (bug in the pump). ~20 Rust lines added to `wl-globals`.

**Also relevant, and not yet excluded:** our libudev entries must carry a seat assignment
matching whatever `libinput_udev_assign_seat` is called with (`seat0`), or libinput skips
them. **Input to a compositor has never once been demonstrated in this project** — M4's
original mission was exactly this and it died at PRIME/dmabuf before any client saw an event.

### 7. Only a raw `wl_shm` client renders — no libcosmic/iced app draws

**"Runs but renders nothing", and the contrast is the evidence.** `wlclient` — no toolkit,
raw `wl_shm` — draws **instantly** through this compositor. `cosmic-settings` is **staged,
launched, alive with 3 pids, owns its D-Bus name, logs nothing, and paints 0 px at +6 s,
+26 s and +74 s.** Same compositor, same protocol, same machine. That localises the gap to
**libcosmic/iced + `tiny-skia`**, not to Wayland and not to the kernel.

This is the **same shape already recorded for cosmic-panel** — presenting fresh buffers while
rendering nothing into them, with the kernel and cosmic-comp exonerated by measurement. Treat
those two as **one investigation**, because they probably are.

### 8. Nothing to launch, and no way to ask for it

**One 2,867-byte data file disables every system keybinding in the desktop.**
`Action::System(system)` is handled as
`if let Some(command) = self.common.config.system_actions.get(&system) { … }`
(`cosmic-comp/src/input/actions.rs:1016-1021`) — an **empty map is a silent no-op**, no error,
no log line. The map comes from `com.system76.CosmicSettings.Shortcuts/v1/system_actions`,
whose upstream default is `../cosmic-epoch/cosmic-settings-daemon/data/system_actions.ron`
defining **26 actions** including `Launcher`, `AppLibrary`, `Terminal`, `WorkspaceOverview`,
`Screenshot`, `WindowSwitcher`, `LockScreen`, `LogOut`. **`mkfs` stages no `/usr/share/cosmic/`
tree at all** — verified, `grep` for `system_actions` and `share/cosmic` both return nothing.
The symptom is already in the logs as `NoConfigDirectory`, from both the shortcuts config and
cosmic-panel.

**Why this is the highest-leverage cheap row:** `cosmic-launcher`, `cosmic-app-library` and
`cosmic-workspaces` are **built, staged, and successfully launched every single boot** — 12
`launch_pad` starts, max 1 per name, **zero restarts**, four boots, both arches — and all
three are permanently invisible, purely because the only thing that can raise them resolves to
nothing.

**The config half is DONE (`52665aa`), and the item as written was wrong by one file — the
more important one.** `system_actions` was never the whole story: `defaults`
(`cosmic-comp/data/keybindings.ron`, 6,925 B) **is the entire keybinding table**, and
`shortcuts::shortcuts()` falls back to `Shortcuts::default()` — an **empty HashMap**
(`cosmic-settings-daemon/config/src/shortcuts/mod.rs:35-38`). Without it cosmic-comp has **no
bindings at all**, so `system_actions` could never have been reached even had it been staged.
*Two* missing files, and the one this item named was the second of them.
**What landed:** all **263 files / 13 components / 101,353 B** that upstream's own install
rules place under `share/cosmic`, sourced from `../cosmic-epoch` (submodules at
`epoch-1.3.0`) — four recursive trees plus three hand-installed *renamed* files (`.ron`
stripped; `keybindings.ron` → `defaults`). Resolution is
`find_data_file("<name>/v<N>")`, so it tests for the **directory**, and each key is a **bare,
extensionless** file holding one RON value (libcosmic `cosmic-config/src/lib.rs:203,236,481-487`).
**Absent directory ⇒ `system_path: None` ⇒ every lookup returns `NoConfigDirectory`, which
`Error::is_err()` classifies as NOT an error (`:120-123`) — which is exactly why this cost
nothing to miss and produced no failure anywhere.**
**Measured, fresh images, both arches:** `system_path: None` **3 → 0**;
`system_path: Some("/usr/share/cosmic/…")` **0 → 3**; the shortcuts `NoConfigDirectory` **1 →
0**; `Panel Entry Error: NoConfigDirectory` **1 → 0**. The four panel errors that remain are a
*different class* — `GetKey("padding_overlap")`, `GetKey("keep_style_on_maximize")` — because
upstream ships 22 keys for a 24-field struct, so they appear on a correct install too.
**Keybindings still do not fire, and that residual is cleanly attributed rather than guessed.**
The lane built a **control**: `Super+F9` bound in the *user* config to `touch /tmp/kb-f9`,
which needs **nothing** from `/usr/share/cosmic`. It fails too. So the remaining failure is
**item 6**, not the staging — and that control is what makes this a completed fix with a known
blocker downstream rather than an inconclusive one.

**And then there is still nothing to run.** No terminal exists among 175 `/bin` names, and the
image contains **exactly one** `.desktop` file — our own applet stub. `cosmic-term`,
`cosmic-files`, `cosmic-edit`, `cosmic-store` are all in `../cosmic-epoch` and **none is
built**. Even with input and shortcuts fixed, `Super` would open a launcher over an empty
index. **Item 8 is therefore two pieces of work, and the config file is only the first.**

### 9. Nine panel applets have no scoped-out dependency and are simply unbuilt

`cosmic-panel`'s default config names **16 unique applets**; **one** is present, and it is our
~230-line stand-in (`leandros-applet`, `wl_shm` + `xdg_toplevel`, ticking clock) wired in by
the single staged `.desktop`. The other 15 resolve to `exec: None` and are never spawned
(`wrapper_space.rs:460-520`).

Five of the fifteen — Audio, Bluetooth, Network, Battery, Power — need PipeWire/NM/UPower/
logind and are **correctly out of scope; do not count them as gaps.** The remaining nine have
**no scoped-out dependency**: `CosmicPanelWorkspacesButton`, `CosmicPanelAppButton`,
`CosmicPanelLauncherButton`, `CosmicAppletTiling`, `CosmicAppletMinimize`, `CosmicAppList`,
`CosmicAppletTime` (the real one), `CosmicAppletInputSources`, `CosmicAppletStatusArea`.
The build recipe already exists (`m6-session-bins/build-rust.sh`) and the panel's spawn path is
**proven end-to-end** by the stand-in. **Cost is build time, not design** — but note they are
libcosmic/iced apps, so **item 7 gates whether they would render**, and the three buttons are
also the natural triggers item 8 is missing. These three items interlock.

### 10. busd has no D-Bus activation — structural

`<servicedir>` is **deliberately omitted** from busd's `session.conf`, so **nothing is
activatable**: every name must already be owned by a running process. This is what makes
`xdg-desktop-portal-cosmic` — and therefore **screenshots, file choosers and screencast** —
not a "build it" problem. Compounding it, the portal's PipeWire dependency is **non-optional**
in exactly the way already documented for the settings daemon (63 external `pw_*` symbols, no
feature gate); unlike the daemon, the portal actually *uses* the streams, so the existing inert
stub would not suffice. The committed architecture already names reference `dbus-daemon` as
the fallback if busd proves immature — **this is the decision point that would trigger it.**

### 11. Two permanent COSMIC source patches — the goal is totally unmodified

Policy is in *Goal* (Standing context): **temporary debug edits to COSMIC are allowed and
encouraged**; permanent ones are not, and every permanent fix belongs on the LeandrOS side.
These two are permanent and must be retired or justified.

**`ports/cosmic-session/0001-env_rx-timeout-fallback.patch` — the real one.** It races
`env_rx` against a 5 s timeout and falls back to `WAYLAND_DISPLAY=wayland-1`. It **fires every
boot** (`handshake did not complete`), so **no child process ever receives the environment
cosmic-comp exports** — every one launches with only `WAYLAND_DISPLAY=wayland-1
XDG_SESSION_TYPE=wayland`. That is a live functional difference on every session, not a
cosmetic patch, and its recorded cause — "a tokio-integration residual" — is **asserted, never
demonstrated**. **Root-cause it on our side.** The handshake is a socket exchange over
`COSMIC_SESSION_SOCK`; the plausible failure surfaces are our `SCM_RIGHTS`/`AF_UNIX`
implementation, `socketpair` semantics, or fd inheritance across `execve` — all ours, all
testable in `scmtest` (currently **32/0**), which is where a guard for it would live.
**Worth suspecting a shared cause with item 6:** a session whose env handshake never completes
and a compositor that ignores input are both "cosmic-comp is running but not talking to
anything", and it would be a mistake to assume they are unrelated before checking.

**`ports/cosmic-greeter/0001-locker-idle-without-logind.patch` — the cheap one.** Makes the
greeter idle instead of locking at boot, in the absence of logind. greetd + cosmic-greeter are
**explicitly out of scope**, so the honest resolution is probably to stop shipping
cosmic-greeter at all rather than to carry a patch for a component we do not want — which
would also remove one of the 12 launched processes. Check what else expects it first.

(`ports/busd/current-thread-runtime.patch` is **not** in scope here — busd is ours.)

### Ordering, and why

1. **Item 6 (input)** first, unconditionally. Windows already map, composite, stack and draw a
   focus ring; every one of those becomes *usable* the moment input lands, and several rows
   below cannot even be tested without it.
2. **Item 8's config file** next — one `mkfs` entry, and it is what makes three
   already-running components reachable.
3. **Item 7 (iced renders nothing)**, jointly with the panel gap, because it decides whether
   items 9 and the rest of the suite are worth building at all.
4. **Item 9's nine applets**, once 7 says they will draw.
5. **Item 10** only when a portal-shaped capability is actually wanted.

### Corrections this survey forces

- **"Run COSMIC *unmodified*… No COSMIC source patches" is not true of the tree.** Three
  patches exist under `ports/`: `cosmic-session/0001-env_rx-timeout-fallback.patch`,
  `cosmic-greeter/0001-locker-idle-without-logind.patch`, and `busd/current-thread-runtime.patch`
  (busd is ours, so that one is fine). **The session patch is still firing every boot** —
  `handshake did not complete` — so every session runs on a 5 s fallback and **no child ever
  receives cosmic-comp's exported environment**; its recorded root cause ("a tokio-integration
  residual") is asserted, not demonstrated. The *intent* — don't fork COSMIC to make it work —
  is clearly still honoured; the claim of zero patches is what is false. **Second absolute rule
  in one day that a two-command check refutes** (the first was "the repo is Rust-only").
  **Resolved as policy, not by dropping the rule** (see *Goal* in Standing context): temporary
  debug edits to COSMIC are **allowed and encouraged** for investigation, and must be reverted
  with the revert *proven* by a clean `git -C ../cosmic-epoch status --porcelain`. The end
  state is totally unmodified COSMIC. That makes these two patches a **standing debt with a
  deadline rather than a grey area** — the greeter one is scoped-out territory and cheap to
  argue, but the session one is load-bearing, fires every boot, and has an unproven cause. It
  becomes **item 11**.
- **`cosmic-session` is built `--no-default-features`, which switches off `autostart`** as well
  as systemd/logind. The XDG autostart scan is compiled out, not merely inert.
- **The deferred-list entry claiming synthetic sysfs has "no current consumer"** is at best
  unproven now that input is the top item. The libudev shim's static table means sysfs is
  *not* the immediate cause, but the entry's justification should not outlive the measurement.
- `cosmic-idle`'s pure-idle fade was root-caused above the kernel (cosmic-comp recomputes
  `is_inhibited` only on repaint, so a static desktop never re-arms the timer). **The recorded
  mitigation was for our applet to commit ~1 fps — it now ticks at 1 Hz, and whether the fade
  works post-clock is recorded nowhere.** Cheap to re-check.
- Two live defects have observations but **no root cause**: `cosmic-files-applet`'s
  `smithay-clipboard` thread panicking on `Failed to create memory pool: OutOfMemory (12)`
  (process survives), and `cosmic-notifications`' intermittent
  `Failed to setup panel dbus server … Broken pipe`, present on control kernels too.

---

### Next steps, in the order they are worth doing

1. **The Venus host CAN photograph a scanout — SOLVED (`f1bf200`), and what is left is
   narrower than this item used to say.** The blocker was never a QEMU limitation:
   `egl-headless` is a *converter, not a pixel sink*. Its `egl_scanout_flush`
   (`ui/egl-headless.c`) ends in `egl_fb_read(edpy->ds, &edpy->blit_fb)` + `dpy_gfx_update()`
   — a real GL→CPU readback into the 2D console surface, done so a paired 2D listener can
   consume it, as `egl_is_compatible_dcl()` states outright. Nothing was attached to consume
   it. Attach one:

   `-display egl-headless -vnc 127.0.0.1:9,display=venusgpu`

   with the device line otherwise unchanged. **The frame comes over RFB, not `screendump`.**
   Instrument: `python3 -u artifacts/venuscap.py <arch> <out.ppm>`, which runs the positive
   control, then `drmsmoke --hold`, waits for `DRMSMOKE: HOLD READY`, captures, and censuses —
   all on one held serial connection, never touching QMP.
   **Measured, and it is an exact match on both arches**, cross-footed by an independent
   re-census of the PPMs rather than by re-running the capture script: x86_64/KVM 1920x1080 →
   `0xff0000` **65,536**, `0x181818` **2,008,064**, **2 distinct colours**; aarch64/TCG
   1280x800 → **65,536** / **958,464**, **2 distinct colours**. Both boots opened with
   `nosuchbinary_xyz42` confirmed *failing*.

   **Two recorded facts were wrong, and both were mine to correct.**
   (a) *"A virgl-backed scanout is a GL scanout with no `DisplaySurface`"* — **false.** It has
   one, correctly sized and full of real pixels: `virgl_cmd_set_scanout()`
   (`hw/display/virtio-gpu-virgl.c:584`) calls `qemu_console_resize()` **first**, then
   `dpy_gl_scanout_texture()`, which sets `console->scanout.kind = SCANOUT_TEXTURE`. The gate
   is `qemu_console_surface()` (`ui/console.c:1488-1496`), which returns `NULL` for every kind
   except `SCANOUT_SURFACE`. So **`screendump` can never photograph a Venus session under any
   `device=` argument** — it refuses pixels that are physically present. That is *structural*,
   not a missing surface, and it means `a2f9fb6`'s `id=venusgpu` did not open the door it
   looked like it opened. Verified in QEMU 11.0.1 source, both hunks read directly.
   (b) *"`--venus` deliberately keeps std-VGA for OVMF/Limine's GOP"* — **x86_64 only.** On
   aarch64 `virt` there is no implicit VGA at all (the guest logs
   `Framebuffer console resolution: 0x0 pitch=0`) and venusgpu is the only console, so the
   console-0 trap that fooled two measurements **cannot arise there**.

   **★ AND THE VULKAN CLIENT IS NOW PHOTOGRAPHED TOO — M4 IS COMPLETE ON x86_64**
   (`aabab88`, `132d4df`). The carry-over was an inference when the paragraph above was
   written; it is now a measurement. `drmsmoke` reaches the scanout via dumb-BO/2D, `vkwl`
   via cosmic-comp compositing a `wl_shm` buffer Mesa filled by memcpy (plain `sw` — Venus
   reports no `VK_EXT_external_memory_host`), and the *same unmodified* `egl-headless` + VNC
   route photographed both.
   **The discriminator was designed before the capture** and is recorded in
   `artifacts/notes/m9-vkwl/precommit-pass-criteria.txt`. `vkwl` cycles 6 clear colours, so
   the run was set to 304 frames because frames 302/303 land on the two whose 8-bit UNORM
   conversions are least ambiguous — **predicted `0x2666f2` and `0xf2cc1a` in advance**.
   Result, three captures in one boot at 1920x1080: control (desktop up, `vkwl` not started)
   **0 and 0**; seq 302 **151,868 px of `0x2666f2`, 0 of the other**; seq 303 **151,868 px of
   `0xf2cc19`**. Bbox `(721,163)-(1198,480)` = 478x318, fill **0.9991**, 98.87% of the 480x320
   swapchain extent; the 1.13% is COSMIC's rounded corners plus its 1 px active-window border.
   `0xf2cc19` vs the predicted `…1a` is one LSB of UNORM rounding, inside the stated ±2.
   **Two independently predicted colours landing in the same rectangle, each absent from a
   same-boot control, is what makes this evidence rather than "it looks like a desktop."**
   Independently re-censused from the PNGs with a separate decoder: counts scale exactly 4:1
   at half resolution (37,967 × 4 = 151,868), control zero for both.
   Reproduce with `python3 -u artifacts/m9_vkcap.py /tmp/m9vk 304 150 90`.
   **Still open:** aarch64 (step 2), and the `SET_SCANOUT_BLOB`/`SCANOUT_DMABUF` path, which
   remains untested by the same reasoning that used to apply to Vulkan.
2. **aarch64: Vulkan-to-scanout is now PROVEN (`8634425`, `dc013c0`); M4 proper is still
   open.** The recorded blocker — "a COSMIC session on softpipe under TCG is impractically
   slow" — was true of the *COSMIC route* and was never checked against any other. It should
   have been: `drmsmoke --hold` had **already** been photographed on aarch64 under `--venus`,
   so the aarch64 capture path was proven, and `vkrender --present` drives Vulkan straight to
   a scanout with **no compositor at all**. Composing two facts already in this file gives an
   aarch64 Vulkan photograph without ever starting COSMIC. **The expensive part was the
   compositor, not aarch64 Vulkan** — and the measurement says so outright: the full
   51-subtest run reached its present hold **7.1 s** after the command was sent, on TCG.
   **Result, one boot, three captures, all 1280x800, all fully accounted.** Control (no DRM
   client had run): 3 colours, `0x000000` 985,722 / `0xffffff` 38,135 / `0xcd0000` 143 — a
   text console holding **none** of the client's colours. `vkrender --present`: `0x181818`
   **958,464**, `0x0000ff` **47,104**, `0xff0000` **18,432**, non-background bbox exactly
   512..767 × 272..527 at fill **1.0000**. `drmsmoke --hold` in the *same boot* afterwards
   reproduced its pinned 65,536/958,464 frame, which is what shows the camera was working.
   **The criterion that makes this unfalsifiable-by-luck was pre-committed** in
   `artifacts/notes/m9-vkrender-aarch64/precommit-pass-criteria.txt`, and it is the best
   discriminator this project has used: the same run prints
   `s2_coverage: triangle=18432 clear=47104 other=0` from a **CPU-side readback of the
   rendered image, before any of it touches DRM**, and P7 required the scanout census to equal
   that triple *exactly*. It does. 18,432 + 47,104 = 65,536 = the whole 256x256 image, and
   `s2_checksum` came out at the pinned `0x02C0FDC5`. Re-censused here independently.
   **Scope, stated plainly because it is easy to overclaim.** This route is
   `vkCmdCopyImageToBuffer` → host-visible memory → dumb BO → `SETCRTC`. **No swapchain, no
   WSI, no `vkQueuePresentKHR`, no dmabuf, no compositor.** It does **not** substitute for
   x86_64's M4. `vkwl` on aarch64 remains unrun, and **n = 1** — one boot, not repeated.
   Also unverified: the box did not rebuild its kernel, so this ran against a kernel predating
   `883e33d`; immaterial here (that commit only touched `readlinkat`) but stated rather than
   assumed.

   **★ AND THE WSI HALF IS NOW DONE TOO — M4 IS COMPLETE ON BOTH ARCHES** (`991151f`,
   `d91edbf`). `vkwl` reaches `vkQueuePresentKHR` on aarch64 inside a **full COSMIC session**
   and its pixels are photographed. `vkCreateSwapchainKHR` → `VK_SUCCESS`, 480x320, 5 images,
   `requested=28 acquired=28 presented=28`, exit 0. 28 frames was chosen because
   `28 ≡ 4 mod 6`, putting the last two on `cols[2]`/`cols[3]` — **the same two colours
   x86_64 used**, so the censuses compare directly. Criteria were committed *before* the first
   capture (`artifacts/notes/m9-vkwl-aarch64/precommit-pass-criteria.txt`) and added one
   x86_64 did not have: both colours must land in the **same** bbox.
   Result: control **0 / 0**; seq 26 **151,868** `0x2666f2`; seq 27 **151,868** `0xf2cc19`;
   bbox 401..878 × 128..445 = 478x318, fill **0.9991**, 98.87% of extent, 0 byte-swapped, 0
   uncovered. **Every comparable number is identical to x86_64's**, and aarch64's seq 27 is
   *cleaner* — zero residue of `cols[2]` where x86_64 had 14,750. Re-censused here
   independently. Cross-feet: all six cycle colours zero in the control, and the panel clock
   reads 00:00:47 / 00:02:40 / 00:05:14 across the three captures, so they are three distinct
   live frames.
   **The "impractically slow" blocker was wrong by three orders of magnitude, and had never
   been measured at all.** Host-side, from sentinel arrival: launch → `wayland-1` bound is
   **1 s** standalone and **2 s** for the full session; `vkwl` start → 28th present ~3-6 s.
   The whole cost is the compositor *settling*, 6-26 s from present sentinel to pixels.
3. **If a permanent Vulkan-Wayland test is wanted, write it in Rust — but NOT the way this
   file previously prescribed, because that recipe cannot work.** `vkwl.c` stays outside the
   repo, at `~/code/leandros-artifacts/venus-lane/vkwl.c` on the host (and on the box), and is
   **not versioned** — an accepted consequence of the Rust-only rule, not an oversight. Treat
   it as a **throwaway probe that has already delivered its finding** (M4's client half). The
   same applies to the other host-side C probes (`vktest.c`, `ssp_guard.c`, `caps_probe.c`,
   `wlclient.c`) — scaffolding, and they stay out. **Do not teach the build system a C path.**

   **The recorded recipe — "a `no_std` Rust crate built by `scripts/build-userland.sh`,
   alongside the other `userland/*test` binaries" — is unachievable, and the reason is
   structural rather than a detail to work around.** `build-userland.sh` emits only *static*
   binaries (`-C link-arg=-static -C relocation-model=static`, lines 66-67 and 76-78), and a
   static binary **cannot `dlopen`**: relibc's `dlopen` returns `NULL` with
   `"dlfcn not supported"` whenever `Tcb::current()` is `None` or `tcb.linker_ptr` is null
   (`../relibc/src/header/dlfcn/mod.rs:95-106`), which is exactly the static case. `vkwl`
   exists *because* it `dlopen`s the ICD — the Vulkan loader is deliberately unshipped and the
   ICD can never stand in for `libvulkan.so.1`. So the prescription contradicts the binary's
   whole reason for existing. **A plan can be rule-compliant and still impossible; this one
   satisfied both standing rules and could never have produced a working binary.**

   The recipe that *does* work is `wl-globals`/`leandros-applet`'s: a dynamically linked musl
   PIE with a real `PT_INTERP`, `-C target-feature=-crt-static -C relocation-model=pic`,
   against `m3-gl-stack/sysroot-<arch>`. **Neither of those crates is in this repo** — both
   live in `~/code/leandros-artifacts/` (`m9-wlglobals/`, `m7w-applet/`) and the repo only
   *stages their prebuilt binaries*, conditionally, at
   `scripts/mkfs-f2fs-populated.py:728-750`. So "write it as a `userland/` crate" has **no
   in-tree precedent to copy**: either `build-userland.sh` grows a third, dynamic-musl mode,
   or the crate sits in `userland/` deliberately excluded from the workspace with its own
   `.cargo/config.toml`. Both are new shapes. Cost that honestly before scheduling it.

   **Two further constraints, both new and neither obvious.** (a) The pure-Rust
   `wayland-client` backend that `wl-globals` and `leandros-applet` use **will not do**:
   `vkCreateWaylandSurfaceKHR` needs a real libwayland `wl_display*`/`wl_surface*` because
   Mesa's `wsi_wl` marshals on it with `wl_proxy_*`. `vkwl` would be the first client here to
   need `wayland-client`'s `system` feature, which links `libwayland-client` through a `cc`
   build script — the most likely place a cross-compile breaks, and unexercised in this
   project. (b) `ash` can take the ICD entry point directly via
   `Entry::from_static_fn(StaticFn { get_instance_proc_addr })`, so no loader is needed; it
   requires `std`, which is fine on musl and impossible on `*-unknown-none`. Estimated
   **~450-650 lines with `ash`**, ~1300-1800 hand-rolling the FFI.

   **What `vkwl.c` actually is, since this file has described it loosely.** 763 lines. It
   binds exactly **two** globals — `wl_compositor` and `xdg_wm_base` — and merely *observes*
   `wl_shm`/`zwp_linux_dmabuf_v1` as flags; **Mesa's WSI binds those itself inside the ICD**.
   The "54 globals" figure is a run *observation*, not a constant, and **"300 frames" is a
   command-line argument — the default is 5.** Load-bearing details a rewrite would silently
   drop: the up-to-40-roundtrip wait for the first `xdg_surface.configure` (without it a FIFO
   swapchain deadlocks on the second acquire), the per-frame `wl_display_roundtrip`, the
   `xdg_wm_base` pong (a missed pong gets the client killed), setting `MESA_VK_WSI_DEBUG`
   *before* `vkCreateInstance`, and the `VKWL_ICD` escape hatch that lets the same binary run
   against RADV on the host.
4. **Fix the framebuffer console scrolling the scanout out from under cosmic-comp** — a real
   bug, found while photographing M4, and the most valuable thing that lane produced besides
   the photograph. The fb console scrolls the **whole** framebuffer on every line, including
   the region cosmic-comp is scanning out. The compositor repaints only damaged regions, so
   anything static is scrolled away and never redrawn.
   **Measured, with a prediction made first.** With `vkwl` logging one line per frame,
   distinct colours collapsed **334,503 → 177**, **79% of the screen went pure black** (the
   wallpaper was simply gone), and the client's four previous frame colours were smeared into
   bands above the current one, each an exact multiple of the 15 px text row. The panel bar
   survived — its clock ticks, so it damages itself every second. The prediction that running
   the client `quiet` would restore the wallpaper, push fill ≥ 0.95 and drop the held count
   toward the extent was **confirmed on all three counts**, and the single 31 px residue band
   left in capture C is exactly the two console lines legible at the bottom of that frame.
   **FIXED (`edad115`), and the root cause was deeper than this item described.**
   The fb console and the DRM scanout **are the same buffer**: `drivers/src/kms.rs:163-169`
   points `BOOT_FB`/`KERNEL_FB` at the RAM surface backing virtio-gpu resource 1, and both
   present paths resolve their destination from that same `get_hardware_fb_info()`. So
   `scroll_vector` (`drivers/src/framebuffer.rs:621`) memmoves the compositor's scanout.
   **A gate already existed and failed two independent ways.** It fired on a *hardcoded ioctl
   list* — `cmd == 0x1001 || 0x1004 || 0xC06864A2 || 0xC01864B0` — which **never included
   `DRM_IOCTL_MODE_ATOMIC` (`0xC03864BC`)**. Since `6edc295` made COSMIC take the atomic path,
   an atomic compositor scanned out with the console **fully live**: that is the reported bug,
   and it was a gap in an allow-list rather than a missing feature. Separately,
   `interface.release()` fired on *any* card0 close and reclaim does `fb.clear(0)` plus a
   banner, so a short-lived second open of card0 wiped a live compositor.
   **The fix claims the console from the present itself, not from an ioctl number.**
   `SCANOUT_WRITES` (`drivers/src/drm/device.rs:11`, bumped at `:363` and `:437` — exactly
   where a client writes the shared surface) is sampled across each dispatch by `handle_ioctl`
   (`drivers/src/drm_device_interface.rs:2008`, `:2098`), which then calls
   `drm_scanout_claim(open_id)`; ownership lives in `framebuffer.rs:674-762`. **A present path
   added later is covered without being listed anywhere** — which is precisely the failure mode
   of the list it replaces. Known cost, documented in the commit: two card0 opens issuing
   ioctls concurrently on two vCPUs can misattribute a present, costing the console one early
   close; a KMS master is single here, and threading `open_id` through would remove even that
   at ~9 signatures.
   **A second, unrelated bug fell out:** `/dev/fb0` read the *wrong buffer* on x86_64. KMS
   repoints `BOOT_FB`/`KERNEL_FB` to a RAM surface (virtio-gpu cannot DMA from OVMF's VGA VRAM
   BAR) but never told the VFS, so reads returned a frozen snapshot of the boot console
   (`servers/vfs/src/lib.rs:1524`, `drivers/src/kms.rs:170`).
   **Falsified by mutation, both arches**, via new `drmsmoke` subtest
   `CONSOLE_YIELDS_TO_SCANOUT` — take the scanout, fingerprint through `/dev/fb0`, open+close
   a second card0 fd, print 160 lines, fingerprint again, require byte-identity. Same test
   binary both builds; only the kernel gate differs. Gate removed: aarch64
   `12209877486060206593 → 2373313276106322956` FAIL, x86_64
   `617618239335486853 → 16756505720239424604` FAIL. Gate present: identical, PASS. The
   `before` values match across builds, so the divergence is the provocation and not drift.
   **What still works, checked:** boot messages (owner is 0 until a client presents; console
   text visible at login on both arches); console reclaim on session exit (**byte-identical**
   control↔fixed, aarch64 md5 `675b0773…`, x86_64 `ff004561…`). Panics force-reclaim before
   printing (`kernel/src/main.rs:655`, two atomic stores, no locks, since the panicking thread
   may hold `KERNEL_FB`) — **argued from construction, not exercised**, as there was no way to
   panic on demand. It is nonetheless strictly better than before, when a panic during a DRM
   session reached serial only.
   **The guard's limit is CLOSED (`c8cbbc1`), and the three-way mutation proves it separates
   the halves.** `drmsmoke` now drives a real plane-only `DRM_IOCTL_MODE_ATOMIC` commit —
   `ATOMIC_TEST_ONLY_NO_PRESENT`, `ATOMIC_COMMIT`, `ATOMIC_PRESENTS_PIXELS`,
   `CONSOLE_YIELDS_TO_ATOMIC`, taking the suite 25 → **29/0** both arches.
   **The design choice that makes it discriminate:** the block runs **before `SETCRTC`**, on
   its **own** framebuffer painted a different red. A guard placed *after* `SETCRTC` cannot
   discriminate at all, because `SETCRTC` is on the old allow-list — the console is already
   silent by then, so the atomic commit proves nothing. `fb0_census` now takes the red it
   expects, so the later `FB0_SHOWS_SCANOUT` still demands the gradient's `0x40`, which the
   atomic present's `0x80` cannot supply; it stays a real self-check rather than being
   satisfied by the earlier present.
   **All ten runs (5 builds × 2 arches) matched the prediction:**

   | build | `…TO_ATOMIC` | `…TO_SCANOUT` |
   |---|---|---|
   | control | PASS | PASS |
   | A — whole gate removed | **FAIL** | **FAIL** |
   | B — allow-list restored, reclaim scoped | **FAIL** | **PASS** |
   | C — `ATOMIC` counted, reclaim unscoped | **FAIL** | **FAIL** |
   | restore | PASS | PASS |

   **Row B is the new coverage**: it passed the old suite and fails now, *while the legacy
   guard still passes* — so the failure names **which half** broke. aarch64 atomic lane held at
   `6277992429985415973` and collapsed to `2373313276106322956` under all three mutations,
   which is the same value the console-scrolled surface took in `edad115`'s own falsification.
   Restore byte-identical to control on both driver files.
   **Layout facts, read from `std_handle_atomic` (`drivers/src/drm_device_interface.rs:2969`)
   and worth not re-deriving.** `objs_ptr` holds bare object ids with **no type tag** — the
   class is recovered from the property id, whose ranges are disjoint per class.
   `SRC_X/Y/W/H` (43-46) are **16.16 fixed point** (the driver does `val >> 16`); `CRTC_*`
   (47-50) are plain. **`ALLOW_MODESET` is not needed**: `changes_modeset` is true only if the
   request *names* `ACTIVE`/`MODE_ID`/connector `CRTC_ID`, so a plane-only commit is legal
   bare — and it works **before any `SETCRTC`**, because `crtcs`/`planes` are populated in
   `DrmDevice::new()` and `handle_flip_page` falls back to `vfs_get_framebuffer_info` when
   `crtc.mode` is `None`. With `damage_blob == 0` the `unchanged` short-circuit cannot fire, so
   every such commit presents.
   **`scripts/scmrun.py` gained an optional third arg**, a completion marker: it returns as
   soon as the marker appears, treating the duration as a ceiling rather than a fixed wait
   (backwards compatible). Six full-surface censuses per run would otherwise have imposed a
   worst-case x86_64/TCG budget on every fast run; a full x86_64 run is now ~6 minutes.

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

### 2. M4 — the client half is achieved; pixels and aarch64 remain

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
CLOSED** (`9d73b43`). **The follow-on claim that "the Venus host still cannot take one" is
now REFUTED** (`f1bf200`) — see next-step 1 for the working invocation and the exact-match
census on both arches. What survives of the original diagnosis is only the `screendump`
half, and even that had the wrong mechanism: bare `screendump` does return a PPM of the
*text console* on x86_64, and `device=` did fail `DeviceNotFound` before `a2f9fb6` added
`id=`. But `"no surface"` afterwards is **not** because the surface is missing — it exists
and holds real pixels; `qemu_console_surface()` refuses it because `scanout.kind` is
`SCANOUT_TEXTURE`. So `screendump` is *structurally* incapable here, and the fix was never
going to be a better `device=` argument.
**The reasoning error worth keeping:** "`screendump` cannot photograph it" was generalised
to "the host cannot photograph it", and the search stopped at the failing tool instead of
asking what `egl-headless` does with the pixels it demonstrably reads back. One tool's
structural limit is not the platform's.

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

**★ M4's client half is ACHIEVED: a Vulkan client presents into cosmic-comp inside
LeandrOS, through Venus, on a real host GPU.** x86_64, KVM, `--venus`, fresh images.
`vkCreateSwapchainKHR` → `VK_SUCCESS` with 5 images, `vkQueuePresentKHR` → `VK_SUCCESS`,
**300/300 presents on a sustained rerun, 0 failures**, against device
`"Virtio-GPU Venus (AMD Ryzen 9 7950X (RADV RAPHAEL_MENDOCINO))"`, api 1.4.307.

**The mutual-exclusivity premise is confirmed on the target, not merely inferred.** `vkwl`
bound **54 globals, `wl_shm` = YES, `zwp_linux_dmabuf_v1` = no**, identically in all three
runs. The `nosw` control fails, as predicted — but **earlier than the plan said, and the
plan's failure mode is wrong**: not `VK_ERROR_SURFACE_LOST_KHR` at swapchain creation, but
at `vkGetPhysicalDeviceSurfaceCapabilitiesKHR`, which returns all zeros
(`minImageCount=0 currentExtent=0x0 supportedUsage=0x0`) and aborts the run there. That
call's `VkResult` was not logged, so the zeroed caps are measured and the **code is not
known** — route reasoning confirmed, specific failure mode corrected.

**Correction, and it reverses what this file said one commit earlier (`77268de`): `noshm` is
NOT required on Venus.** Venus reports `VK_EXT_external_memory_host = no` (RADV reports
yes), and per `wsi_common_wayland.c:3548-3556` a false `has_import_memory_host` makes Mesa
select `WSI_WL_BUFFER_SHM_MEMCPY` **on its own**. So plain `MESA_VK_WSI_DEBUG=sw` is
sufficient *and correct* for Venus, and both routes pass in the guest. `noshm` is a
workaround for drivers that **do** advertise `external_memory_host` — which is exactly why
bare `sw` failed on the RADV host and passes in the guest. **The host measurement was real
but did not transfer, and was briefly recorded here as though it did.** Different driver,
different Mesa (26.1.4 vs 25.3.6): a host control is a client sanity check, not a guest
prediction.

**`vkswap` is not a Wayland client and never was.** It, `vkrender` and `vktest` are all
`VK_EXT_headless_surface`. **No Vulkan Wayland client existed in this project**, so the plan
named a vehicle that could not carry it. `vkwl` was written for this (musl toolchain,
`DT_NEEDED` = `libc.so` + `libwayland-client.so.0`, already staged) and validated host-side
first, which is why a guest failure could never have been blamed on the client.

**Pixels remain UNPROVEN, and that is the honest boundary of this result.** cosmic-comp
logged **no** new-surface/toplevel line naming `vkwl`. The screendump captured **console 0**,
which is the std-VGA that `--venus` deliberately keeps for OVMF/Limine's GOP — it holds the
text console, and the capture legibly confirms the run (`M4: wayland-1 present after 1s`,
`MESA_VK_WSI_DEBUG=sw,noshm frames=300`) but **is not the scanout**. Targeting the right
device failed because `screendump` resolves `device=` as a qdev **id** and `GPU_DEV` is
`peripheral-anon`. That is the *same* missing-`id=` defect already recorded above, now hit
from a second direction. **This does not show COSMIC failed to display — it shows the
capture missed a different QEMU device.**
**Superseded on the route, not on the result** (`f1bf200`). Adding `id=` was *not* the fix:
`screendump` is structurally incapable of photographing a Venus scanout at any
`device=` (`qemu_console_surface()` returns `NULL` unless `scanout.kind == SCANOUT_SURFACE`,
and virgl sets `SCANOUT_TEXTURE`). The host **can** be photographed — pair `egl-headless`
with a VNC consumer and fetch over RFB, exact-match verified on both arches with
`drmsmoke --hold`. So "visual confirmation is not available on this host" is **false as
stated**; what is true is that **`vkwl` itself has still not been photographed**, because
the capture route was proven with a DRM client rather than a Vulkan one. Client-side present
success remains measured and strong. The outstanding step is small and specific: run `vkwl`
under a COSMIC session with `venuscap.py` attached.

**aarch64 is entirely untested** — no baseline, no M4 run. The aarch64 `vkwl` is built and
staged (218,768 B) but never executed. Every number in this item is x86_64.

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
`artifacts/notes/m9-crossopen-dmabuf/crossopen_design.md`.

### 4. The fb console scrolled the scanout out from under cosmic-comp — FIXED

Root cause, fix, mutation evidence and the guard's limit are all in **next-step 4** above.
One paragraph for anyone scanning: the fb console and the DRM scanout **are the same buffer**,
so console scrolling memmoved the compositor's pixels and the compositor — repainting only
damage — never restored them. A gate existed but keyed on a **hardcoded ioctl allow-list that
omitted `DRM_IOCTL_MODE_ATOMIC`**, so it stopped working the moment `6edc295` moved COSMIC to
the atomic path; reclaim was also unscoped, letting any second card0 close wipe a live
session. `edad115` claims the console from *the present itself* (a `SCANOUT_WRITES` counter
sampled across each dispatch) rather than from an ioctl number, so future present paths are
covered without being enumerated.

**Two instrument facts worth carrying forward, both counter-intuitive.** First, **`screendump`
cannot see this bug at all**: `DIRTYFB` points the virtio scanout at the client's own resource
(`Virtio::flush` switches scanout on id change), so corruption of resource 1 stays off-camera.
The in-guest `/dev/fb0` census is the only instrument that can see it — a case where the
*better* camera is blind and the crude one is not. Second, the plumbing self-check
`FB0_SHOWS_SCANOUT` **reported FAIL on x86_64 on its first run** and caught the census reading
a stale buffer; without it, x86_64 would have returned a **vacuous PASS in both builds** —
identical fingerprints because nothing was being read, indistinguishable from the fix working.
That self-check is what forced the `/dev/fb0` fix, and it is the reason the x86_64 number can
be quoted at all.

**Still open:** `drmsmoke` drives no real atomic commit, so the new guard cannot distinguish a
fix that only scoped reclaim from the full one. ~30 lines closes it; details in next-step 4.

### 5. Deferred work and known limitations

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
- **`/proc/self/exe` returns `/bin/init` regardless of the caller — WRONG AS RECORDED, and
  the real defect was next door.** `/proc/self/exe` has worked correctly since `0aefc36`
  (2026-07-21) added the tgid-keyed side table (`sched/src/lib.rs:921-956`), which
  `sys_execve` populates. The item was stale by a fortnight. **What was actually broken:
  `/proc/<pid>/exe` for a *numeric* pid had no handling at all** — it fell through to the
  generic `VFS_READLINK`, found no such file and returned `-ENOENT`. So a caller naming a
  process by pid (including itself) got an error, not a wrong path. Fixed in the working tree
  by generalising the branch at `kernel/src/syscall.rs:5524`: parse `/proc/<digits>/exe`,
  reject a pid naming no live task with `-ENOENT` (`exists_probe` returns 0 when live), map
  pid → tgid, and reuse the lookup `self` already used. **No new kernel state** — the existing
  table had everything, and fork/clone inheritance plus leader-exit cleanup were already
  correct.
  **Falsified by mutation, which is what makes the new subtest worth having.** With the kernel
  hunk stashed and the *same* `epolltest` binary: `proc_pid_exe: path= /proc/5/exe got -1` →
  FAIL, `pass=9 fail=1`, while `proc_self_exe` still **passed** — that control is what proves
  the new subtest is testing the gap rather than some unrelated breakage. Restored:
  `got /bin/epolltest`, `pass=10 fail=0`. Both arches 10/10, `vfstest` 36/36 both arches on
  fresh images, `forktest` 3/3.
  **The lesson is the one this file keeps re-learning:** a one-line deferred item stated a
  symptom nobody re-checked, and the symptom had been fixed while the *adjacent* defect stayed
  open. Re-derive a deferred item's premise before scheduling it — reading it is not enough.
- **libseat shim eventfd workaround** (`0bed5ad`): the *premise* holds, the *conclusion*
  overstated what was available, and chasing it exposed something worse.
  **Premise, verified three ways:** the kernel does honour `EFD_NONBLOCK` now —
  `handle_eventfd` stores `flags & (O_NONBLOCK_FL | O_CLOEXEC)`
  (`servers/vfs/src/lib.rs:4821`, `O_NONBLOCK_FL = 0o4000` at `:1357`), `fd_nonblock` reads it
  back (`:896`), and `sys_read_impl` returns on `-11` instead of yield-spinning when it is set
  (`kernel/src/syscall.rs:4062-4067`). So the hang `0bed5ad` dodged is gone.
  **But "can be simplified" was wrong.** What `0bed5ad` changed was *removing a `read()`*, and
  not reading stays correct regardless: nothing anywhere writes that eventfd (no seatd/logind
  backend exists), so `libseat_dispatch` has no messages to drain either way. The kernel fix
  changes what a hypothetical read would *do*, not whether to do one. **Only the comment was
  stale**, and it is now corrected in the working tree. A fix landing upstream of a workaround
  does not automatically make the workaround removable — check whether the workaround's code
  was ever load-bearing on the bug, or merely contemporaneous with it.
  **Reopened as input suspect #2, then CLEARED — and the clearing is stronger than the original
  reasoning.** The worry was that `libseat_get_fd` hands smithay an eventfd that never signals
  while `enable_seat` fires synchronously inside `libseat_open_seat`, so `LibSeatSession` might
  never emit `SessionEvent::ActivateSession`. It does not matter:
  `smithay/src/backend/session/libseat.rs:88-91` calls `seat.dispatch(0)` then `rx.try_recv()`
  **specifically to catch a synchronous enable** — our shim's contract is the exact case that
  code was written for, so `active` is true from construction. And **nothing on the input path
  reads it anyway**: `process_input_event` and the libinput calloop closure never call
  `is_active()`; its only readers are KMS paths that early-return when inactive and would have
  left every output unmodeset. The desktop renders, so it is active. **No shim edit was made.**
  The original entry's conclusion stands; the reason it gave was weaker than the real one.
- **The input-stack shims were unbuildable and unwired — FIXED in the working tree, and the
  binaries were never actually drifted.** Before: `build-all.sh` never compiled
  `ports/input-stack/shims/` (`grep -iE 'libseat|input-stack|build-shims' scripts/*.sh`
  returned nothing), the image packed a prebuilt unversioned blob from
  `~/code/leandros-artifacts/m4-input-ship/<arch>/usr/lib/`, and the only build script
  hardcoded `D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/d3-input-stack` — a one-off job
  temp directory, unrunnable as checked in. Editing the tracked sources changed nothing.
  Now: `build-shims.sh` is repo-relative (`SCRIPT_DIR`/`ROOT_DIR`, the `build-all.sh` idiom),
  builds through `scripts/linker-<arch>-musl.sh` — the zig-cc wrapper already used for
  coreutils/brush/bottom/relibc, rather than a new one — into
  `target/input-stack-sysroot/<arch>/usr/lib/`, and `build-all.sh` calls it. `mkfs` **prefers**
  the fresh build for `libseat.so.1`/`libudev.so.1` and falls back to the blob. Output stays
  under `target/` deliberately: `~/code/leandros-artifacts` is not a git repo and mixes
  built-from-source with hand-staged blobs for six other libraries, so writing there would
  blur exactly the distinction this fix exists to make.
  **The drift question is answered: there was none.** Rebuilt vs shipped, both arches, both
  shims — identical `SONAME`, identical `llvm-nm -D --defined-only` **and**
  `--undefined-only` symbol sets, every allocatable section matching; only `.debug_line`/
  `.debug_str` differ, because the embedded source path shortened when the build left the job
  temp dir. The blobs really were built from the tracked source. **The hazard was structural
  — unreproducible and unwired — not an actual mismatch.**
  **Verified positively rather than by absence.** Build sentinel `🎉 Build Complete!` present
  once in a 2,646-line log; both arches boot to a login prompt with the `nosuchbinary_xyz42`
  positive control confirmed failing; `anvil --help` reaches its own argument parser on both
  arches, which is a real consumer dynamically linking the rebuilt shims rather than merely
  finding them on disk. **The cross-foot that proves the preference logic actually fired in
  the pipeline** (not just in isolation): `mkfs` packed 11552/64056 on aarch64 and
  10928/63320 on x86_64 — the *fresh* sizes; the blob's are 11704/64136 and 11072/63408, all
  four different. Both follow-ups were tested, not reasoned about: moving
  `target/input-stack-sysroot` aside makes `mkfs` pack the blob's sizes (fallback genuinely
  fires), and stripping `zig` from `PATH` prints
  `⚠️ zig not found on PATH; skipping input-stack shim build` and exits 0.
  **One hazard remains, flagged rather than fixed.** On a machine with **neither** `zig`
  **nor** the `m4-input-ship` blob cache, the image builds successfully and **silently ships
  with no `libseat.so.1`/`libudev.so.1` at all** — `usr_lib_files` simply omits them, and a
  COSMIC session then fails at runtime with nothing in the build saying why. This is *not*
  new: it is the pre-existing shape of every `os.path.exists()`-guarded blob in
  `mkfs-f2fs-populated.py`. It is recorded because it is the "absence looks like success"
  pattern that this file already tracks in five separate instrument entries, sitting in the
  packaging step rather than in a measurement.
  **A small trap met on the way:** `libseat.so.1` is a **16-byte symlink** to
  `libseat.so.1.0.0`. An `ls -l` on it reports the symlink's size and mtime, not the
  library's — which is how a "the blob predates its commit by 4h49m" reading got started
  before symbol comparison replaced it. Stat the target, not the link.
- **The interpolated clock CLAMPS, and every clamped timestamp ends in `9999`.** Both
  `arch/x86_64/src/timer.rs` and `arch/aarch64/src/timer.rs` end `monotonic_ns` with
  `break base + frac.min(9_999_999);`. A saturated stamp is therefore
  `t*10_000_000 + 9_999_999` ns, so **the entire saturated `tv_usec` alphabet is
  `{9999, 19999, …, 999999}`** — `999999` is not a near-max coincidence, it is just the
  once-per-second member. The clamp engages when the **timer IRQ is late**: real elapsed
  already exceeds one tick period while `TICK_COUNT` has not yet incremented. Measured
  **10/48 samples (20.8%) on x86_64/TCG and 0/40 on aarch64/HVF**, because IRQ latency is
  milliseconds under TCG and microseconds under HVF.
  **It is benign** — a clamped stamp is emitted only after real time has passed the tick
  end, so it is *behind* truth, never ahead; `MONO_LAST_NS.fetch_max` keeps the sequence
  non-decreasing; staleness is bounded by IRQ lateness; and `tv_sec`/`tv_usec` come from a
  single `now_ns` read so they cannot disagree. Strictly better than the old clock, which
  was *always* a full tick stale. The one behavioural note: two flips in the same clamped
  tick get **identical** timestamps, so a consumer differencing them can see 0 ms.
  Tick calibration is *not* implicated — guest monotonic time tracked host wall clock to
  <0.3% over two 145 s intervals.
  **The trap, and it is easy to get backwards:** a clamped sample looks like "no signal" but
  is **positive evidence**. The old clock computed `(ticks % 100) * 10_000`, always ≡ 0 mod
  10,000, so it could never emit a value ending in `9999` — reaching the clamp at all proves
  the interpolated path ran. `FLIP_TS_SUBTICK` (`b04b48d`) therefore fails only on
  *all-tick-multiples*, the old clock's exact signature, and counts saturated samples as
  passing with a printed note. Requiring a *genuine* sub-tick sample would be flaky, not
  strict: a measured x86_64/TCG run came in at **15 saturated / 1 genuine**, one sample from
  a spurious failure that would have indicated nothing about the math.
  **Do not space such samples with `usleep`/`nanosleep`:** `sys_nanosleep` rounds any nonzero
  request **up** to whole ticks, so sleeping between flips resyncs to the tick edge and
  *reinforces* the phase alignment that produces all-saturated runs. Use busy-work.
- **Crate layering, worth remembering because it will recur:** `drivers` has **no Cargo edge**
  to the arch crates. The tree's existing answer is a `#[no_mangle] extern "C"` symbol resolved
  at link time — `arch_monotonic_ns`, which `servers/evdev` already uses for input timestamps.
  Reach for that before adding a dependency edge and inverting the layering.
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
  `~/code/leandros-artifacts/m9-wlglobals/out/wl-globals-<arch>` — a HOST path, not the
  in-repo copy: `out/` holds built binaries and is gitignored. Same conditional pattern
  as `leandros-applet`. It is a measurement instrument (it enumerates the `wl_registry` of
  every `wayland-*` socket in `$XDG_RUNTIME_DIR` and exits); nothing in the session depends
  on it. **Both arches are now built and staged**; the crate's `.cargo/config.toml` already
  carried the x86_64 target section, it had simply never been exercised. Needs
  `cargo +nightly` — the default stable toolchain has no Linux musl targets installed.
  **Why, precisely, and it flips the day the crate moves in-tree:** this repo's
  `rust-toolchain.toml` already pins `nightly-2026-04-16` *and lists both musl targets*, so
  a crate **inside** the repo needs no `+nightly` at all. `wl-globals` and `leandros-applet`
  live in `~/code/leandros-artifacts/`, outside that file's scope, and neither carries a
  `rust-toolchain.toml` of its own — verified, not assumed — so they fall back to the
  default toolchain and the `+nightly` is genuinely required *there*. A toolchain pin is
  directory-scoped; do not carry a caveat across a directory boundary without re-checking it.
  **Correction worth keeping:** the `-C relocation-model=static` landmine below does **not**
  apply to this binary. `wl-globals`, like `leandros-applet`, is a genuine *dynamically
  linked* PIE with a real `PT_INTERP` (`/lib/ld-musl-<arch>.so.1`), built with
  `-C target-feature=-crt-static -C relocation-model=pic` against `m3-gl-stack/sysroot-<arch>`.
  The landmine is about Rust's self-relocating *static*-PIE, which is a different recipe.
  Applying it here would break a working build. Both arches verified to have identical ELF
  shape (DYN, 11 program headers, same order).
- **The artifacts tree is now versioned, in `artifacts/`.** It previously had **no version
  control at all** — `~/code/leandros-artifacts` is not a git repo, so `vkwl.c`, `vktest.c`,
  `ssp_guard.c` and every `build-*.sh` had no history and nothing to recover them from for the
  whole Vulkan arc. **Only authored material was imported**: sources, notes, patches and
  harnesses. The 41 GB / 131,505-file original holds vendored upstream trees (Mesa under
  `llvmpipe-lane/deps-*`, pipewire, musl) and build output, which are deliberately excluded
  and must be rebuilt from their recorded recipes; `artifacts/.gitignore` keeps them out.
  **The original directory still exists and was not deleted** — it remains the place where
  builds actually run.
  **`artifacts/` imports no `*.c` or `*.h`**, and `artifacts/.gitignore` blocks them so they
  cannot be reintroduced by a careless drop. The host-side C probes (`vkwl.c`, `vktest.c`,
  `ssp_guard.c`, `caps_probe.c`, `wlclient.c`) stay outside the repo and stay unversioned.
  That is deliberate — they are scaffolding that has already yielded its findings, and the
  standing rule is that a capability worth keeping gets **rewritten in Rust**, not imported
  as C.

  **State that rule accurately, because "the repo is Rust-only" is false and a grep refutes
  it in one command.** `git ls-files '*.c' '*.h'` returns **seven** files at HEAD: two
  vendored (`.limine-cache/…/limine.c`, `limine-bios-hdd.h`), one stray
  (`userland/libc/src/test_mmap.c`), and **four hand-written and actively maintained** —
  `ports/input-stack/shims/{libseat,libudev}/*.{c,h}`, with real commit history such as
  `0bed5ad`. The rule that actually holds, and that the shims satisfy rather than violate:
  **new capability code is Rust; C exists only where the artifact *is* a C ABI.** A shim
  whose entire job is to impersonate `libseat`'s symbol table has to be C — you cannot
  provide a C library's ABI without writing the C ABI. `vkwl` is not that: it is a client
  *of* an ABI, so nothing about it requires C. **Fixing the premise strengthens the rule.**
  Stated as an absolute, the first person to run that grep concludes the rule is dead.
- **Two spent instruments, kept but not pending.**
  `artifacts/notes/m9-damage-rootcause/damage_rect_dump.patch` (132 lines,
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
