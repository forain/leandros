# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`49399f9`), after a five-lane wave that
landed two commits here and four on the Linux box, closed three items by measurement,
**corrected three claims the previous reconciliation had recorded wrongly**, and found one
memory-safety bug that is now the first item in the file.

**Landed on this Mac.** `fe411ff` closes the former TIME_WAIT item: a closed TCP port is
now held for 60 s, `SO_REUSEADDR` became real in the same patch, and the landing is
*provably* live rather than merely staged — a single-variable A/B (same `/bin/scmtest`,
two kernels, `md5 f2fs-data0-aarch64.img = 0c1e090c…` identical on both sides) moved
**exactly** assertion (b), `rc=-1 errno=98` patched against `rc=0 errno=22` with only
`time_wait_add` removed, `scmtest` 31/0 against 30/1. `c27557f` compiles `DRM_STATS` back
out; it had been left `true` by `c5abb8d` against the flag's own doc comment, while
`CURSOR_DEBUG` and `mm::gap2::ON` were already `false`. **`c5abb8d`'s outstanding
`drmsmoke` gate is cleared**, 22/0 on both arches with `idletest` 2/0, run twice — once
with diagnostics on and once in the shipping configuration after the flag flip, which is
the run that counts. `drmsmoke` cannot be moved by that commit's counter redefinition by
construction (`userland/drmsmoke/src/main.rs` never reads `FLIPS_SUBMITTED` or
`[DRMSTAT]`), so any movement would have been a real regression; there was none, and no
revert is warranted. Suite baselines move to **`scmtest` 31/0**, `vfstest` 36/0.

**Landed on the Linux box only,** whose `main` is at `a0325c6`: `0df1810`, `e083202`,
`eccc4e9`, `a0325c6`. `origin/main` is untouched at `6a0eb0c` and nothing has been pushed
from either machine. **The sync onto this Mac is deliberately on hold** — `e083202`
widens the item 1 use-after-free from dumb buffers to blobs, so bringing it over before
the fix trades a bug reachable through one path for the same bug reachable through two.
That is item 2, and it is a decision, not housekeeping.

**The former `SIMULATE_SYNCOBJ` item is closed**, landed on the box as `a0325c6`, with
`venustest` at **91 PASS / 0 FAIL on both arches**. The four subtests that had failed were
re-specified — but *the recorded reason for their failure was wrong in both the previous
report and the item text, and it is corrected here because `git log` cannot be edited*.
It is **not** `RING_IDX` against a virgin context: a `GPU3D_DEBUG` `(size, flags, ring)`
histogram over one boot shows Mesa's own **first** submit of a renderer lifetime
(`size=0x8C flags=0x04 ring=0`) and its `vn_ring_destroy` teardown submit
(`size=0x10 flags=0x06 ring=0`) both **complete**, on a context where no host ring has
ever been created — ring 0 is the CPU ring and needs no creation. The variable is the
**stream**: venustest's 32 zero bytes are not dispatchable (`vkr: vn_dispatch_command
failed`), and with `RING_IDX` the completion fence routes through the renderer context
instead of the global timeline, so a context whose dispatch failed never retires it. Real
Mesa cannot reach this; it needs a host-side dispatch failure. What survives is a
robustness note, item 3, at the strength the evidence supports.

**`vkrender --present` ran**, 10/10 subtests, `vkrender` 0 failures overall,
and it needed **zero code** — it was unrun, not unfinished. The host wire trace shows the
whole handover: `RESOURCE_CREATE_2D 0xb (1920x1080)` → `ATTACH_BACKING` → **`SET_SCANOUT
id 0, res 0xb`** → `TRANSFER_TO_HOST_2D` → full-frame `RESOURCE_FLUSH`, with the console
driver reclaiming the scanout on exit. That run also corrected the item's `screendump`
account: the failure was `DeviceNotFound`, because QMP resolves `device=` as a qdev id and
`--venus`'s device line carried no `id=`. With `id=venusgpu` it works *before* the present
and fails `"no surface"` *during* it, because a virgl-backed scanout has no
`DisplaySurface`. The remaining half of item 6 is a Vulkan-free present tool on the
non-Venus path.

**The former cross-open dmabuf item is resolved as an M4 blocker, by measurement rather
than by argument.** In a live COSMIC session cosmic-comp advertises **54 globals** and
`zwp_linux_dmabuf_v1` is **absent**, as are `wl_drm` and `wp_drm_lease_device_v1` — all
three behind the same `!is_software` gate. M4 therefore goes via `MESA_VK_WSI_DEBUG=sw`:
1–2 days, all userspace, zero kernel days, and the shipped `libvulkan_virtio.so` already
has both WSI branches compiled in, so no Mesa rebuild. Stages 3–5 of that design are
**killed as an M4 unblocker**; Stages 1–2 remain due, because they are item 1.

**Two lanes were still running when this was written**, and neither result may be assumed:
one validating AF_UNIX `listen()` strictness with a dirty-image COSMIC double-run on the
Linux box, one implementing the dmabuf-lifetime fix. **Both have since reported — see the
final-wave paragraph below; the numbers cited in this paragraph are the numbering of that
moment, not of this file.** What each outcome means is written into
its item.

Earlier waves this session, compressed: `05f7279` (aarch64 kernel softfloat — six separate
items trace back to that clobber, directly or as the cause that retired them), `531f21e`
(harness prompt detection), `4085b7f` (nested epoll), `75b32e3` (sub-tick
`CLOCK_MONOTONIC`), `26eebf0` (AF_INET loopback), `05bb0fe` (evdev monotonic timestamps),
`77f170d` (memfd tmpfs-slot leak + TGID canonicalisation), `b2260b4` (`run-qemu.sh
--venus` + `vkrender` staging — the first GPU work ever submitted from LeandrOS),
`9be954f` (`import_fd` EMFILE double-release, use-after-free class), `07d461c` (repeat
`listen()`, dead `init-server` crate), `97a979e` (subtest comments stop citing TODO item
numbers), `c5abb8d` (`FB_DAMAGE_CLIPS`), `18a7a9f` (blob cacheability), plus this wave's
`fe411ff` and `c27557f`. **Fifteen code commits on this Mac (TODO-only commits aside);
four on the box.** The item count
does not fall much across the session because analysis kept finding pre-existing defects
that were always there and simply unmeasured, not because work ran out — item 1 is the
sharpest example.

**Final wave of 2026-08-06, and the state this file now describes.** Two further lanes
closed and the two trees were reconciled. The AF_UNIX `listen()` strictness patch
**landed** (`055745f`) after the validation it had been held for: eight COSMIC session
boots — control double-run and patched double-run, both arches — showed no restart loop,
12 `launch_pad` starts with max 1 per name, identical control against patched, and zero
`Unknown id`, `PANEL MAIN ERR`, `panicked` or `restarting process` in any run. The
negative control is what makes it evidence rather than absence: an unpatched kernel
running the *same* patched `scmtest` gives 30 PASS / 1 FAIL on `unix_listen_strict`, its
diagnostics naming all five must-fail assertions reading `rc=0`. **Carry this caveat
forward:** `XDG_RUNTIME_DIR=/run/user/0` is a `TMPFS_ROOTS` entry and is verifiably empty
at every boot, so a reboot cannot carry a stale `S_IFSOCK` into the next session — the
double-run tested a dirty `$HOME` (proven by a marker file and run 1's full
`com.system76.Cosmic*` tree), **not** dirty sockets. The specific route to a failing
`bind()` that this patch was feared to escalate is unreachable across a reboot on this
system; the same-boot route was exercised for 240 s / 600 s per run with nothing dying.

The use-after-free is **fixed and landed** (`49399f9`), and the two trees are **no longer
divergent**: `e083202`, `eccc4e9`, `a0325c6` and `3532c7b` were cherry-picked from the box
(as `3dbba0c`, `3d4c980`, `09def61`, `055745f`), and `0df1810` was **skipped** because its
patch-id is byte-identical to this Mac's `18a7a9f` — the same change had reached the two
trees by different routes, and only `git patch-id` says so, since the SHAs differ. The
lifetime fix needed one hand-written integration edit on top: `blob_map_cache_type`, which
arrived with blob cacheability, iterated `BLOB_BUFFERS` reading `map_phys`/`size`/
`map_info`, and those fields now live on the object. It iterates `BLOB_OBJS` instead,
because they describe the host mapping — which belongs to the buffer, not to any one
handle naming it, and once import mints a second handle a per-handle scan could disagree
with itself.

**A tenth instrument lied, and it was a shell pipeline.** `git apply --check … | head -10
&& echo OK` reports success whenever `head` succeeds, because a pipeline's status is the
*last* command's — so a patch that failed printed `APPLIES CLEAN` two lines below its own
error text. Branch on the command itself (`if git apply --check …; then`) rather than
piping it. This is the same shape as the other nine: it failed toward looking successful.

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment *unmodified* (source: `../cosmic-epoch`)
on both x86_64 and aarch64 under QEMU. No COSMIC source patches; build-configuration
flags (`--no-default-features`) are allowed. Everything beneath COSMIC — kernel, libc,
system libraries, daemons — is ours. **This constraint is load-bearing twice over**: the
primary-plane over-damage is inside `OutputDamageTracker`/`DamageShaper` (item 8), and
the missing dmabuf global is behind cosmic-comp's `!is_software` gate (item 7). In both
cases the reachable outcome is a measurement or an upstream reproducer, not a patch.

**Where it stands.** The desktop runs on both arches: cosmic-session → cosmic-comp on
KMS/softpipe → busd → cosmic-bg + cosmic-panel renders a wallpaper plus a full-width
panel bar with an embedded Wayland client, clock ticking. Remaining desktop work is
quality and performance, not bring-up. Vulkan runs **and presents**: `vkrender` executes
fill-buffer, compute and graphics work, `vkswap` drives a headless-surface swapchain to
`vkQueuePresentKHR -> VK_SUCCESS`, and `vkrender --present` puts a rendered image on a
real DRM scanout.

**Suite baselines.** On fresh images with `vfstest` run exactly once per image, both
arches: vfstest **36/0**, scmtest **31/0**, drmsmoke **22/0**, wakepolltest 10/0,
forktest 3/0, epolltest 9/0, polltest 6/0, sigtest 6/0, timertest 6/0, memtest 4/0,
idletest 2/0 (`IDLE_CPU_US 0`), evtest2 8/0. `waittest` is **5/0 or 3/2 on either arch**
— a pure timing race in `fork` → child `setpgid(0,0)`+`_exit` → parent `waitpid(-pid)`,
measured on pristine kernels too; either result is acceptable on either arch, and the
x86_64-vs-aarch64 asymmetry seen in any single wave is noise (this wave: aarch64 3/2,
x86_64 5/0). On a **Venus host** (the Linux box, `--venus`), at the box's `a0325c6`:
`venustest` **91/0 both arches**, `vktest` 14/0, `vkrender` **51/0** with
`s2_checksum = 0x02C0FDC5` pinned across x86_64/KVM, x86_64/TCG and aarch64/TCG, `vkswap`
**21/0** (x86_64). `vkrender` under KVM **no longer needs** `VN_PERF=no_fence_feedback` —
that dependency died with `18a7a9f`/`0df1810`.

**A Mac `venustest` run is worth nothing, in either direction.** QEMU 11.0.2 on macOS has
**no blob-capable virtio-gpu device at all**: `virtio-gpu-pci,blob=on` is refused with
*"need rutabaga or udmabuf for blob resources"*, and neither `virtio-gpu-gl-pci` nor any
rutabaga variant is compiled in. `VIRTIO_GPU_F_RESOURCE_BLOB` is never advertised, so no
blob BO can be created and nothing downstream of one can be exercised. A Mac `venustest`
reports **42 lines, 11 PASS / 31 FAIL**, byte-identical on patched and unpatched kernels.
Do not compare that against the box's numbers and conclude anything. Everything blob-,
HOST3D- or Venus-shaped goes to the box.

**cosmic-comp offers no dmabuf to clients here — measured, not inferred.** A live aarch64
session at `c27557f` advertises 54 globals on `/run/user/0/wayland-1`, identical across
three passes 30 s apart; `zwp_linux_dmabuf_v1`, `wl_drm` and `wp_drm_lease_device_v1` are
all absent, and no `wayland-1-card0` socket exists, so `create_socket`
(`cosmic-comp/kms/socket.rs:31`) was never called and `is_software` is true. **Scope:**
absent *in this configuration* — software EGL, forced because the macOS host has no EGL,
so `virtio-gpu-gl,venus=on` is unusable and the guest has no hardware GL. It would flip
only if the guest gained a non-software EGL device. Full report and controls:
`~/code/leandros-artifacts/notes/m9-crossopen-dmabuf/stage0a-wl-globals.md`.

**Instrument reliability — read this before trusting a number.** **Nine** separate
instruments have now produced believable wrong numbers, or would have:

1. A parser keyed on field *position*: `m8_cursor.py`'s regex ran from `flip_us` onward,
   and `c5abb8d` inserted five `dmg_*` fields between `flip_us` and `curs_up`, so every
   field after the insertion point silently read **0** on a patched kernel. Parse
   `key=0xHEX` pairs order-independently (`m9_analyze.py` does).
2. A guard test that passed with its guard removed: `memfd_inflight_close` as first
   written could not fail, because the hazard window never opened. The same trap was
   walked into and *avoided* later — a `close(0)`-consequence check would have been
   vacuous, since `sys_fcntl` short-circuits `fd <= 2` and answers `F_GETFD` with a
   hardcoded `0` without consulting the fd table.
3. A serial `expect()` that searched backwards over an accumulated buffer and re-matched
   the *previous* command's end sentinel: every command after the first reported `rc=0`
   **without ever running**. Caught only because a log claiming `venustest` passed
   contained no `venustest` output. Take the buffer mark *before* sending, and number the
   sentinel per command.
4. `driver.py cmd`'s shell-prompt heuristic swallowing error lines on TCG x86_64, where
   the guest is slow enough for the heuristic to break early.
5. A count delta that looked perfect: `virtio_gpu_cmd_ctx_submit` events per renderer
   lifetime came out 6 on HEAD and 7 patched — a clean +1. The same binary three times in
   one HEAD boot gave **6, 6, 7**. Venus notifies its ring opportunistically; the count
   floats and the +1 was noise.
6. **`grep` over a serial log sharing a pty with QEMU's trace stream.** With
   `-trace virtio_gpu_cmd_*` and no `-D`, every guest character triggers a console flush,
   so trace lines land *between* the guest's bytes: `present_addfb2: PASS` arrived as
   twenty single characters. `grep -a "present_"` found **2 of the 10** present subtests
   and reported nothing wrong — the eight missing ones looked exactly like eight subtests
   that had never run. The same shredding broke the harness's own sentinel, so a `rc=0`
   run was reported as a harness failure. Fix: `-D <file>`, so the trace stream never
   touches the pty.
7. **The Stage 0a instrument that was caught before it lied.** The natural build — extend
   `leandros-applet` to dump the registry — would have enumerated **cosmic-panel's
   embedded server**, because the panel hands each applet an inherited `WAYLAND_SOCKET` fd
   and `connect_to_env()` follows it. That server advertises `wl_compositor`, `wl_shm` and
   `xdg_wm_base` and no dmabuf: **indistinguishable from the true negative being hunted,
   and it passes the "the other globals prove the dump worked" sanity check**. The lesson
   worth keeping is general — *a sanity check can be satisfied by the very failure it was
   meant to exclude*, so an instrument must establish **which thing it measured**, not
   merely that it measured something. `wl-globals` does: it ignores the environment,
   globs `wayland-*` in `$XDG_RUNTIME_DIR`, connects by explicit path, and the identity of
   the socket is pinned by cosmic-session's own
   `got environmental variables from cosmic-comp: [("WAYLAND_DISPLAY", "wayland-1")]`.
8. A positive control that came back showing **only the prompt**, because the read window
   raced login settle. Re-running it passed — which is itself the argument for running a
   control rather than assuming one would have passed.
9. **Commands sent to a shell ~180 s after a COSMIC session launch do not execute** (or do
   not echo): the console is saturated by session output. A probe that types its
   measurement later returns nothing. Any session-probing design must **background its
   work early**, while the console is still responsive — the Stage 0a dumper was launched
   as the third command and slept 100 s inside its own process, which is the only reason
   that measurement exists.

Two rules follow, and both are cheap. **Run a positive control** — send a known-failing
command (`nosuchbinary_xyz42`) as the first command of every boot and confirm the harness
reports it failing; that single step catches 1, 3, 4 and 8. **Prefer a structurally
distinctive observable over a count delta**: replacing a submit *count* with a
`(payload size, flag word, ring index)` histogram settled the syncobj question
unambiguously — 16 bytes occurs zero times in 72 submits across five HEAD lifetimes and
exactly once per lifetime patched, always last before `ctx_destroy` — where the count had
hidden the event entirely. Cross-foot every number against a second, independent source:
the test binary's own `failures = N` trailer caught a `^\S+: PASS$` extractor that
reported `PASS=0` because the serial console emits CRLF.

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
  `validate_user_buf`. Either pair it with `prefault_user` (private to the syscall
  crate) or hoist the copy above the lock so the fault happens with nothing held.
- **The kernel is softfloat on both arches and must stay that way.** The EL0 trap
  frame saves no vector state, so any kernel code LLVM lowers through a vector
  register lands on the interrupted thread's. Both kernel target JSONs disable the
  vector units; `cpu_switch_to` is the single deliberate exception and scopes the
  extension with `.arch armv8-a+fp+simd` … `.arch armv8-a`. Six items across this
  session trace back to this clobber, directly or as the cause that retired them.
- **A borrowed VMO's page list is immutable.** Stated and enforced by the PRIME export
  commit (`e083202`, box-only — item 2). It closes three hazards measured *live* on this
  Mac, not theoretical ones: an unpatched kernel returned a valid mapped address for a
  page past the frames the DRM layer lent it, accepted an 8-byte `write()` into it, and
  *succeeded* at shrinking a borrowed frame list — order-0 frees out of an order-N buddy
  block. Until that commit reaches this Mac, the Mac tree still has all three.
  **The converse invariant does not exist yet and is item 1**: nothing makes the DRM
  object outlive an exported dmabuf fd.
- Release builds only — debug builds crash early. Test **both** arches in QEMU after
  every change. Minimum Limine revision is **6**, never downgrade.
- Regression images must be freshly regenerated — run vfstest **exactly once** per
  freshly generated image. A dirty f2fs image produces phantom failures
  (`chroot_confines_symlink_resolution`, `xattr_list_tmpfs`, `xattr_list_f2fs`). The
  historical aarch64 `xattr_list_f2fs` red has not appeared anywhere this session, on
  either machine, consistent with it being that artifact and not an arch bug.
- **A guard test must be shown to fail with its guard removed, or it is certifying a
  hazard it never checked.** See the instrument-reliability entry above; a test that
  cannot fail and an instrument that cannot report failure are the same defect.
- **Subtest comments must not cite TODO item numbers.** Six did, and this file gets
  renumbered as items land — every citation had drifted within one day. Point to the
  defect or the commit instead; those don't move. **Three violations are outstanding**,
  found by `grep -rn "TODO.md item\|TODO item [0-9]"` at reconciliation time: the prepared
  `driverpy_venus.patch` cites "see TODO.md item 4/12" twice (patch lines 24 and 101) and
  must be edited before it lands; and `userland/vfstest/src/main.rs:1` ("item #4") and
  `userland/f2fstest/src/main.rs:1` ("item #5") survived the `97a979e` sweep — both now
  point at items that have not existed for months. Two one-line comment fixes, deliberately
  not folded into this reconciliation commit.

**Memory attributes, measured rather than assumed.**

- *aarch64.* `MAIR_EL1` arrives as a flat **`0x00000000000000ff`** under Limine 11.4.1 —
  attribute 0 is `0xFF` (Normal WB/WA) and **attributes 1..7 are all zero**, i.e.
  Device-nGnRnE. `18a7a9f` installs **index 2 = `0x44`** (Normal Inner/Outer
  Non-cacheable) with a read-modify-write in `mmu::enable_identity`, before `arch::init`
  maps anything and before `smp_init` snapshots MAIR for the APs, and prints
  `[ARCH] MAIR_EL1 before=… after=…` (`arch/aarch64/src/lib.rs:84`) so the inherited
  value stays visible. Index 3 (`ATTR_NOCACHE`) is Device-nGnRnE and always was; index 1
  (`ATTR_DEV`) is too — item 5. The aarch64 framebuffer is therefore Device memory and is
  **deliberately left that way**; it works only because `pitch = width*4` keeps every
  access aligned.
- *x86_64.* Limine 11.4.1 **does** program `IA32_PAT`, to `0x0000_0105_0007_0406`
  (PA0 WB, PA1 WT, PA2 UC-, PA3 UC, **PA4 WP**, **PA5 WC**, PA6 UC, PA7 UC), decoded from
  a `mov ecx,0x277` / `wrmsr` site in `BOOTX64.EFI+0x42f34` guarded by
  `CPUID.01H:EDX.PAT`. `BOOTAA64.EFI` has zero such sites. Only our direct-boot path
  (`kernel/src/entry_x86_64.s`, which writes EFER and nothing else) leaves the reset PAT.
  `18a7a9f`'s commit message says the reset PAT applies; that is wrong on the Limine
  path, though it reaches the right conclusion anyway because PA2 is UC- in both tables.
  This is a static decode of a binary and has **not** been confirmed by a runtime read —
  item 4 adds the print that would.

**Diagnostics in-tree, all `false` at HEAD** — flip to `true`, measure, flip back before
committing. `c5abb8d` shipped with `DRM_STATS` on and `c27557f` had to undo it; the rule
is not decorative.

| Flag | File | Measures |
|---|---|---|
| `DRM_STATS` | `drivers/src/drm_device_interface.rs:1344` | flips, cursor up/mv, atomic, atest, cplane, `dmg_{full,rect,skip,px}`, `blobs`, `evpush` |
| `CURSOR_DEBUG` | `drivers/src/virtio_gpu.rs:342` | cursor queue setup + selftest |
| `mm::gap2::ON` | `mm/src/gap2.rs:17` | memfd/MAP_SHARED path + frame checksum sampler |
| `pci::RENDER_DEBUG` | `drivers/src/pci.rs:99` | per-frame DRM/FB/GPU/KMS/SND serial tracing |

The one diagnostic that is **not** behind a flag is `[DRM-SRV] mmap …` — item 10.

**`RUST_LOG=trace` cannot read smithay's own damage-tracking decisions.**
`cosmic-comp/Cargo.toml:61-62` sets `release_max_level_info` on `tracing`, so `trace!`
calls are compiled out of the release build and the feature ceiling cannot be raised
additively. Kernel-side counters are the only instrument, and the `FB_DAMAGE_CLIPS` blob
is the damage tracker's **verbatim** output (`PlaneDamageClips::from_damage`, smithay
`backend/drm/surface/mod.rs:68-100`, is a 1:1 `map` with no splitting or merging), which
is what makes the kernel-side decode a real measurement of a client-side decision.

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
| 1 | The blob half of the dmabuf lifetime fix is unexercised | Verification | a Venus host |
| 2 | `driver.py` fakes a clean run when `aarch64_vars.fd` is missing | Bug — harness | — |
| 3 | A host-refused `RING_IDX` submit costs a full control-queue timeout | Finding — kernel/host | — |
| 4 | x86_64 `IA32_PAT`: the BSP and the APs disagree — fix prepared | Bug — kernel | — |
| 5 | aarch64 `ATTR_DEV` is Device-nGnRnE, and a landed comment implies otherwise | Bug — comment | — |
| 6 | Vulkan presents headless and to a scanout; M4 goes via `MESA_VK_WSI_DEBUG=sw` | Feature | — |
| 7 | Cross-open dmabuf import — dead as an M4 route, alive for other reasons | Feature — deferred | — |
| 8 | Primary-plane over-damage is upstream; only a measurement remains | Perf | — |
| 9 | `kms_swrast` destroys imported handles with `MODE_DESTROY_DUMB` | Bug — kernel | — |
| 10 | The `[DRM-SRV] mmap` trace is unconditional and floods a session | Bug — diagnostics | — |
| 11 | Deferred work and known limitations | Mixed | — |

---

## Prepared but not landed

Three patches remain. All were re-checked `git apply --check`-clean against this Mac's
`c27557f`; the two that have since landed are gone from this list, not marked done.

1. `~/code/leandros-artifacts/notes/m9-x86-pat/pat_bringup.patch` — 331 lines, 3 files,
   all under `arch/x86_64/src/`. Builds all four kernel variants. **Never run.** Touches
   nothing any other lane owns. See item 4.
2. `~/code/leandros-artifacts/notes/m9-damage-rootcause/damage_rect_dump.patch` — 132
   lines, one file, `drivers/` only, entirely inside the `DRM_STATS` gate, **built** for
   both targets. Prints the decoded damage rect list. Optional; see item 8 for what it
   would and would not settle.
3. `~/code/leandros-artifacts/notes/m9-dmabuf-lifetime/dmabuf_lifetime.patch` — superseded
   as a patch (landed as `49399f9`), kept only because its companion `dmabuf_lifetime.md`
   is the reference for the refcount model. Do **not** re-apply it.

Also prepared, not a kernel patch:
`~/code/leandros-artifacts/notes/m9-driverpy-venus/driverpy_venus.patch` — teaches
`.claude/skills/run-leandros/driver.py` a `--venus` mode with the exact device line
`run-qemu.sh --venus` uses, refusing `--venus` on non-UEFI boot modes and on macOS rather
than degrading. Applies clean. **Two edits before it lands:** remove the TODO-item
citations (standing rule), and correct the docstring's `screendump` account — the reason
`device=` failed was `DeviceNotFound` from a missing `id=`, not `"no surface"` (item 6).

---

### 1. The blob half of the dmabuf lifetime fix is unexercised

The use-after-free itself is **fixed and landed** (`49399f9`): an exported dmabuf fd now
holds a reference, one per gem handle and one per exporting `TmpVmo` slot. What remains
is that half of its regression coverage has never run.

`venustest` phase 6's nine **blob** assertions emit nothing on a host without blob
support, and this Mac's QEMU refuses `blob=on` outright (`need rutabaga or udmabuf for
blob resources`; udmabuf is Linux-only, rutabaga is not compiled in, and there is no
`virtio-gpu-gl-pci`). The `mmap(MAP_SHARED)` half of the hazard — writing *into* recycled
memory, as opposed to reading it — is blob-only and is therefore covered nowhere in any
run to date.

What **is** proven, on aarch64, by mutation: removing the dumb arm's
`b.refs = b.refs.saturating_add(1);` makes `phase6_dumb_payload_survives_destroy` fail
with `dumb payload lost at offset 0`, `close(fd)` emit `[DRM] bo refcount underflow
obj=0x00000001`, and the retire path stop firing — three independent signals from one
line, on a byte-identical image (`f2fs-data0` `3dfc0004…`, `venustest` ELF `c002f94e…`
across a kernel-only rebuild). Failures present as **wrong values, not panics**: the
frames stay HHDM-mapped and merely belong to someone else, so a panic there means
something else is wrong.

The churn loop is load-bearing and was measured rather than argued: `buddy::free` does
not scrub, so a bare `read()` after `GEM_CLOSE` would often return the original pattern
and the test would pass against its own bug. In the mutated arm, churn allocation #1
received `0xB82D2000` — the exact page freed one event earlier — and zeroed it.

**To close:** run `venustest` phase 6 on the Linux box, where `blob=on` works. Expect the
nine blob assertions to emit and pass, and the blob arm's own refcount mutation
(`o.refs` in `prime_export_acquire`) to produce the same three signals.

**Also unstress-tested:** the leak watch found retained-but-fd-pinned dumb records at
**0** and live dumb buffers at **4**, frozen over 38 census samples across 185 s of live
COSMIC with a running clock — but cosmic-comp exports dumb buffers **per allocation, not
per frame**, so the retention path was never entered. The change is inert in that
workload rather than proven under it. Note the failure direction: this change can only
make buffers live *longer*, so its failure mode is a leak where the previous failure mode
was memory corruption.

### 2. `driver.py` silently fakes a clean run when `aarch64_vars.fd` is missing

`driver.py` auto-creates `x86_64_vars.fd` but **not** `aarch64_vars.fd`. Without that
file QEMU exits instantly, `driver.py start` still prints `QEMU started`, and the serial
log is 0 bytes — so every subsequent test reads as *absent* rather than *failed*, which
in a suite that greps for PASS lines is indistinguishable from a run where nothing was
asserted. It bit a fresh worktree during the dmabuf verification and was caught only by
the positive control (`nosuchbinary_xyz42` → `ConnectionRefusedError`); without it, four
empty result files would have been read as a clean sweep.

Workaround in place: `cp /opt/homebrew/share/qemu/edk2-arm-vars.fd aarch64_vars.fd`.
The fix is to make `driver.py` create it the same way it creates the x86_64 one, and to
fail loudly when the serial socket never opens rather than reporting a started guest.

### 3. A host-refused `RING_IDX` submit costs a full control-queue timeout

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
renderer lifetime carries `RING_IDX` on it and completes. A genuinely **nonexistent** ring
index was not tested — our driver bounds-checks `ring_idx` against the context's
`num_rings` before it could get that far. Also **not separated, and not claimed either
way**: whether the non-retiring fence is a property of *that* submission's failed dispatch
or of a context already poisoned by an earlier one; venustest's failing case always ran on
a context that had already had a stream rejected. Both readings give the same answer to
the question that mattered.

**Real Mesa cannot reach this** — its streams are valid Venus protocol, and `vktest`,
`vkrender` and `vkswap` issue dozens of `RING_IDX` submits per boot with zero timeouts. So
this blocks nothing. It is recorded because it is a denial-of-service shape available to
any future client that submits a malformed stream with `RING_IDX` — which is every Mesa
submission — and because a caller cannot tell it apart from a dead host. Whether the right
answer is a guest-side precondition, a shorter timeout with a distinct error, or nothing
at all is **undecided; this is a finding, not a plan.** Do **not** "fix" it by refusing or
rewriting `RING_IDX` kernel-side: Mesa sets it on every submit.

### 4. x86_64 `IA32_PAT`: the BSP and the APs disagree — fix prepared

**The item this replaces had a false premise.** "There is no `IA32_PAT` bring-up in
`arch/x86_64/`, therefore the reset PAT applies" — the first clause is true, the second is
false on the boot path we actually use. Limine 11.4.1 programs `IA32_PAT` and already puts
**WC at PA5** (decode in Standing context). WC has been one PTE bit away the whole time.

**The reason to act is a live cross-CPU divergence on `main` today, not throughput.**
`IA32_PAT` is per-logical-processor and an AP leaves INIT/SIPI with the **reset** PAT. So
on the Limine path the BSP runs `PA4=WP, PA5=WC, PA6=UC` while every AP runs
`PA4=WB, PA5=WT, PA6=UC-`. Limine's framebuffer mapping selects PA5. `arch::init` tries to
re-map the framebuffer `NO_CACHE`, but does so with `map_4k`, which returns `false` the
moment it meets one of Limine's huge pages — the loop says so in a comment and then
ignores the result. The console writes through Limine's HHDM mapping
(`drivers/src/framebuffer.rs:653`). If that re-map fails, **the console is WC on the BSP
and WT on every AP** — one set of physical lines under two memory types on two processors,
which the SDM leaves undefined. It has not bitten us because WT is coherent and the
console is idempotent, but it is the shape of bug that appears as rare corruption rather
than a fault. **Inferred, not yet observed:** the "if that re-map fails" step is a code
reading, and check (b) below settles it in one boot.

**The prepared fix** (`pat_bringup.patch`, 331 lines, 3 files, builds all four kernel
variants, **never run**) makes every CPU agree: the BSP publishes its whole 64-bit PAT and
each AP writes it verbatim. `init_pat_bsp()` is the first statement of `arch::init`, ahead
of the GDT and of everything it maps; `init_pat_ap()` is the first statement of
`smp::sched_ap_entry`, at which point the AP has touched only its stack and parameter
block, both WB through PA0, a slot nothing changes. The slot is **PA5**, chosen so the
write is provably inert on the primary path: under Limine PA5 is already `0x01`, so the
read-modify-write of byte 5 is value-identical and cannot reinterpret a live translation
(including Limine's own PAT-bit framebuffer mapping); on the direct-boot path PA5 goes
WT → WC and provably has no users, since reaching PA4..PA7 requires the PAT bit and the
2 MiB PDEs `entry_x86_64.s:133` builds are `0x83` with bit 12 clear. PA1 (Linux's slot)
was rejected because it is selected by PWT alone and we inherit Limine's page tables
wholesale — we cannot grep a binary's PTEs for an inherited WT mapping.

**Verify in this order.** (a) The boot print `[ARCH] IA32_PAT before=… after=… wc=1`, the
direct analogue of the `MAIR_EL1` print, which converts the static decode into a runtime
read. Expected `before=0x0000010500070406` unchanged on the Limine path,
`0x0007040600070406 → 0x0007010600070406` on direct boot; **any other `before` means the
safety-case split must be re-checked before landing**, and `wc=0` means the CPU or
hypervisor refused the write and we fell back to PCD/UC-, which is `18a7a9f`'s behaviour
and not a failure. (b) `paging::debug_walk_pte(cr3, mm::phys_to_virt(fb_base))` — a 2 MiB
leaf with bit 12 set means the divergence above was live; a `PT[...]` entry with `0x10`
set means `arch::init`'s re-map succeeded and it never was. Either answer is worth
recording. (c) The only check that measures a *win*: time a ~1 MiB `memcpy` into a mapped
host-visible blob with and without `PAT_WC_READY` forced false. UC should be roughly
20-50× slower; within noise means guest PAT is not reaching the hardware and the patch is
inert — a finding, not a bug.

**MTRRs cannot defeat this on the hardware we verify on.** The blob lives in a 64-bit
prefetchable BAR above top-of-RAM, where firmware leaves `MTRRdefType = UC`, and
(MTRR=UC, PAT=WC) is WC. Corroborated twice, because a recalled SDM table row is not
evidence: Linux's `arch_phys_wc_add()` adds no MTRR when `pat_enabled()`, so every DRM WC
framebuffer on Linux gets WC from PAT alone over an MTRR-UC range; and `pat_x_mtrr_type()`
consults MTRRs only for WB requests. The verification host is a Ryzen (SVM/NPT, no
memory-type field in nested paging, guest PAT used directly). The one configuration that
could defeat it — an old-KVM Intel host with EPT `IPAT=1` — would equally defeat the
already-landed UC mapping in `18a7a9f`, which demonstrably works.

Note that WC is **weakly ordered where UC was not**. That moves us toward the reference
behaviour rather than away from it (the host explicitly asked for `VIRTIO_GPU_MAP_CACHE_WC`,
so Mesa's Venus path is written against WC semantics on native Linux, and its ring
submission goes through a locked atomic that drains the WC buffers), but it is worth
knowing if a blob ever gets a new consumer.

### 5. aarch64 `ATTR_DEV` is Device-nGnRnE, and a landed comment implies otherwise

The runtime `MAIR_EL1` read shows a flat `0x00000000000000ff`: Limine took the path that
writes `0xFF` flat, not the `0xFF | (dev_attr << 8)` one, so **attribute 1 is zero too**.
`PageDescFlags::ATTR_DEV` therefore selects Device-**nGnRnE**, not the Device-nGnRE the
flag has always been described as. Behaviourally harmless — nGnRnE is strictly stronger
and MMIO works — but two comments landed in `18a7a9f` are now known to be imprecise:
`arch/aarch64/src/paging.rs:19` still says "device; whatever the loader left" where the
value is now measured, and the `ATTR_NOCACHE` doc block at `:24-32` repeats the
pre-measurement claim that Limine writes `0xFF | (dev << 8)`. Correct both to what the
register actually contains. A comment fix with no code change, listed separately because
the same class of wrong comment (`ATTR_NOCACHE`, "index 3 (normal NC)") cost a full
analysis pass to discover and would have produced an alignment fault had the blob work
reused it.

### 6. Vulkan presents headless and to a scanout; M4 goes via `MESA_VK_WSI_DEBUG=sw`

**A Vulkan swapchain exists on LeandrOS.** `vkswap` — a ~450-line dependency-free C
program in `vkrender`'s idiom (no Khronos loader; `dlopen("/usr/lib/libvulkan_virtio.so")`,
bootstrap from `vk_icdGetInstanceProcAddr`, device entry points via `vkGetDeviceProcAddr`)
— goes surface → present-capable queue family → caps/formats/present modes → device with
`VK_KHR_swapchain` → swapchain (256x256, 5 images) → acquire against a real fence → a
genuine `UNDEFINED → PRESENT_SRC_KHR` barrier submitted on the queue and fence-waited →
`vkQueuePresentKHR -> VK_SUCCESS`. **21 PASS / 0 FAIL** on the box. The layout transition
is deliberate: presenting an `UNDEFINED` image is undefined behaviour, and a present that
skipped it would be a spec violation that happens to return `VK_SUCCESS`. Attribution is
the cleanest in the wave — the same binary on a kernel with the PRIME commit reverted
gives 16/1, and the single failure is `create_swapchain` (`VkResult(-10)`).

**`--present` ran, and the blit reaches the scanout.** 10/10 `present_*` subtests,
`vkrender` `rc=0`, `failures = 0`, and **zero code changes** — it was unrun, not
unfinished. The QEMU wire trace shows the complete device-level handover:
`RESOURCE_CREATE_2D res 0xb, 1920x1080` → `RESOURCE_ATTACH_BACKING` → **`SET_SCANOUT id 0,
res 0xb`** → `TRANSFER_TO_HOST_2D` → full-frame `RESOURCE_FLUSH`, with the console driver
reclaiming scanout 0 (`res 0x1`) when `vkrender` exits — a second, independent
confirmation that the scanout really had been handed over. There is nothing left between
that and photons except the host display backend.

**What is still missing, and the `screendump` account corrected.** The last hop — that the
bytes in the presented resource are the rendered triangle rather than garbage — is
unproven, because **this host cannot photograph it**. Bare `screendump` works and returns
a valid 1920x1080 PPM, but its content is the text console: three colours (`#000000`,
`#ffffff`, brush's `#cd0000`) and **not one `0x181818` pixel**, where `--present` paints a
`0x181818` field with the 256x256 render centred. The earlier "`device=` fails with no
surface" note is **only half right**: the first failure was `DeviceNotFound`, because QMP
resolves `device=` as a **qdev id** and `--venus`'s device line carried no `id=`. With
`,id=venusgpu` added, `device=` **works before the present** (capturing Limine's stale
1280x800 boot surface) and fails `"no surface"` only *after* the guest sets a scanout,
because a virgl-backed scanout is a GL scanout with no `DisplaySurface`. That is a
host-tooling limit, not a LeandrOS defect. **The remaining half of this item** is a
standalone, **Vulkan-free** dumb-buffer present tool run on the **default (non-Venus)**
`run-qemu.sh` path, where the GPU is a plain virtio device with a real `DisplaySurface`;
bare `screendump` then captures it and a `0x181818` field plus a known pattern is trivially
checkable. That separates "does the DRM present path put pixels on a scanout" (answered:
yes) from "does this Venus host have a photographable display" (answered: no).

**M4 goes via `MESA_VK_WSI_DEBUG=sw`, not via cross-open dmabuf.** cosmic-comp does not
advertise `zwp_linux_dmabuf_v1` here (Standing context; item 7), and Mesa's WSI binds
`wl_shm` *only* in the `sw` case and `zwp_linux_dmabuf_v1` *only* in the non-`sw` case —
mutually exclusive (`wsi_common_wayland.c:1406-1421`), so a non-`sw` Venus on this
compositor returns `VK_ERROR_SURFACE_LOST_KHR`. The `sw` route is **1–2 days, all
userspace, zero kernel risk**, and it needs no Mesa rebuild: the shipped
`venus-lane/stage-aarch64/usr/lib/libvulkan_virtio.so` was built
`-Dplatforms=wayland -Dvulkan-drivers=virtio`, contains the `MESA_VK_WSI_DEBUG` string and
its flag table, and has **both** WSI branches compiled in (`wsi_wl` ×30, `wl_shm` ×4,
`wl_shm_pool` ×2, `zwp_linux_dmabuf` ×8). Not yet run end-to-end — that is the next step,
not a finding. It is also the correct bisection point if the dmabuf route is ever
attempted: it proves the client, the protocol, the compositor wiring and the Vulkan
rendering independently, leaving the kernel as the only new variable.

`vkrender` still passes 3/3 subtests with `s2_checksum = 0x02C0FDC5` pinned byte-identically
across x86_64/KVM, x86_64/TCG and aarch64/TCG — set `VKRENDER_EXPECT_CHECKSUM=0x02C0FDC5`,
because the value is **printed but not asserted** unless that variable is exported, and
every comparison so far has been done by hand.

**Environment, still true.** The box is `forain@172.16.158.150`,
`/home/forain/Projects/leandros`, EndeavourOS (**Arch, not Debian**), virglrenderer 1.3.0,
QEMU 11.0.1, Mesa 26.1.3, host GPU a Ryzen 9 7950X iGPU (RADV RAPHAEL_MENDOCINO). aarch64
there needs `-cpu max,lpa2=off` (the Limine 11.4.1 FEAT_LPA2 wedge). macOS has no EGL and
no blob-capable device at all — see Standing context. The loader stays unshipped: the ICD
exports only `vk_icdGetInstanceProcAddr`, `vk_icdNegotiateLoaderICDInterfaceVersion` and
`vk_icdGetPhysicalDeviceProcAddr`, so it can never stand in for `libvulkan.so.1`.

**Build findings, load-bearing for anyone rebuilding these binaries.** `-std=c11` does
**not** compile against musl — strict ISO hides `clock_gettime`, `nanosleep` and
`CLOCK_MONOTONIC`; use `-std=gnu11`. Vulkan headers need `/usr/include/vk_video` as well
as `/usr/include/vulkan`, copied to a private dir — do not point `-I` at `/usr/include`,
it shadows the target libc's headers. The container recipe **cannot build aarch64 on the
box**: no docker, and podman pulls arm64 images but cannot execute them. Cross-compiling
with `zig cc` + `musl-dyn-link.sh` works, with two gotchas — zig cc enables UBSan by
default (link fails on `__ubsan_handle_*`, needs `-fno-sanitize=undefined`) and its driver
silently produces a **static** binary, which cannot `dlopen` the ICD. Corrected recipes:
`~/code/leandros-artifacts/notes/m9-m3-vulkan/build-vkrender-alpine-fixed.sh`,
`build-vkrender-aarch64-zig.sh`, and `m9-vkswap/build-vkswap-alpine.sh`.

### 7. Cross-open dmabuf import — dead as an M4 route, alive for other reasons

`open_may_reach` (`drivers/src/drm_device_interface.rs:1093`) deliberately scopes BOs to
their owning DRM open, which is correct for `b80ab5a`'s ownership model but blocks
`VK_KHR_display` and Wayland dmabuf, both of which import into a different open (and, for
Wayland, a different process).

**Stages 3–5 of the design are killed as an M4 unblocker, by measurement.** cosmic-comp
advertises no `zwp_linux_dmabuf_v1` on a software renderer here (Standing context, with
the scope caveat), so no amount of kernel work reaches a Wayland Vulkan client in this
configuration — the missing global is upstream of the kernel entirely. M4 goes via
`MESA_VK_WSI_DEBUG=sw` (item 6). **Stages 1–2 were never about M4 and remain due: they are
item 1.**

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

Design, staging and per-stage guard tests with their falsifying mutations:
`~/code/leandros-artifacts/notes/m9-crossopen-dmabuf/crossopen_design.md`.

### 8. Primary-plane over-damage is upstream; only a measurement remains

**The kernel side is done and landed** (`c5abb8d`): `DrmDevice::present_damaged`
(`drivers/src/drm/device.rs:411`) copies just the sub-rectangles a `FB_DAMAGE_CLIPS` blob
names instead of scaling and flushing the whole surface, with rects clamped and degenerate
ones dropped rather than the commit rejected — a bad clip list is a hint we are free to
ignore, and failing there would stall the compositor. It also added `DAMAGE_{FULL,RECT,
SKIP,PX}` and `BLOBS_CREATED` to `[DRMSTAT]`. Judge it on the kernel-side defect it fixes
— that a skipped primary still cost a full-screen scale plus full-screen `TRANSFER_TO_HOST`
and `RESOURCE_FLUSH` — and **not** on flips/s, because there is no perf headroom to
recover. **Its `drmsmoke` gate is now closed**: 22/0 on both arches at the merged HEAD, and
again at `c27557f` in the shipping configuration with `DRM_STATS` off. No revert warranted.

**Two claims in `c5abb8d`'s commit message and in the diagnostic that produced it are
wrong, and are corrected here because `git log` cannot be edited.**

- The report said `dmg_skip = 0` "directly confirms" that we fail smithay's third skip
  condition (`age > 0 && last_state.old_damage.len() >= age`). **It does not follow.**
  `dmg_skip = 0` means the tracker never returned *empty* damage; that is independent of
  `age`.
- The headline "~7,800× over-damage" is a cumulative-window artifact and misleads. The
  per-interval data shows **damage tracking demonstrably works when idle**: exactly
  **40,960 px per present = 1280x32 = the panel bar**, for 86 continuous seconds. The
  age-0 fallback fires **twice, at bring-up (t ≈ 4 s), producing exactly 1,024,000 px** —
  its unmistakable fingerprint, since all three of smithay's "damage everything" branches
  push one rect equal to `output_geo` — and **never again in 176 s**.

**The age hypothesis is refuted on two independent lines.** From source:
`Swapchain::acquire` (`allocator/swapchain.rs:154-181`) calls `create_buffer` only inside
`if free_slot.buffer.is_none()` and no path drops a buffer on the way out; cosmic-comp
holds at most two slots (`QueueState::WaitingForVBlank` gates the next render), so the
steady state is a two-slot rotation with `age = 2` and `old_damage` at 2-3 entries against
`MAX_AGE = 4` — the condition is *satisfied*. The only resets are error arms that `bail!`
and produce no flip. From data: the burst value is **992,000 px, not 1,024,000**, and no
fallback branch can emit that number.

**What actually happens is `DamageShaper`, and the arithmetic is exact.** For a full-output
bbox at 1280x800 the shaper's tile grid is 4 x 8 = **32 tiles of 320x100**.
`992,000 = 31 x 32,000` — 31 of 32 tiles. `981,760 = 992,000 − 10,240`, and
`10,240 = 320 x 32` — one tile short by 32 rows at the tile column width. Equivalently:
full width and 767-775 of 800 rows. The idle rect (1280x32) is a `len() == 1` passthrough
with no shaping at all, which is why idle reads clean. The inflation is therefore
**specific to pointer motion**, and the mechanism is the shaper, not buffer age.

**It is not fixable from our side.** Every decision is inside `OutputDamageTracker` and
`DamageShaper`, in the compositor's address space, before a byte reaches the DRM interface;
there is no feedback path from the driver into the damage tracker, and the shaper is
unconditional (`damage/mod.rs:774`), not feature-gated, so `--no-default-features` does not
reach it. A real fix is a COSMIC/smithay source change, which the standing goal forbids.

**What is on the primary plane, established rather than assumed.** Two elements: the panel
layer surface and the wallpaper layer surface (no windows, per the screendump). The cursor
is **not** among them — smithay puts a cursor-plane assignment in a separate slot and only
`overlay_plane_elements` are fed back as fake elements; the one path that could push it
onto the primary is the failed-`test_state_complete` reset, and `atest = 0` for the entire
burst (6 over the whole 176 s), so no test ever failed. `curs_up = 1` and
`curs_mv = 680 ≈ atomic = 684` confirm the plane is live.

**The one thing left worth knowing** is whether the pre-shaper damage set is genuinely
large or a handful of rects inflated into a million pixels. `damage_rect_dump.patch`
(built, unrun) prints `dmg_nrects` plus a bounded rect list and answers it in one run:
`n = 1` full-width at height 767-775 means the bbox shortcut (`shaper.rs:81-88`) and a
pre-shaper set of ≥2 rects one of which is ≥ 892,800 px; `n` in 4…32 with 320 px-wide
strips means the tiled path. If it turns out to be a few small rects inflated to 31 tiles,
that is an **upstream smithay bug report with a reproducer** — a real outcome even under a
no-patch policy. Ceiling: the dump gives the shaper's *output*; the *input* is not visible
from the kernel at all and stays inferred, because `release_max_level_info` compiles out
the `trace!` calls that would show it.

**If that run happens, its own instrument must be checked first.** Assert that parsed `r=`
tuples equal `n` and that the `[/DMGRECTS]` sentinel is present; abort on mismatch. Use the
idle invariant already measured as the built-in positive control — over any 20 s window
with `evpush` delta 0, `dmg_nrects − dmg_rect` must be 0 and `dmg_px / dmg_rect` exactly
40,960. If idle does not reproduce, the instrument is wrong and the motion numbers must be
thrown away. Cross-foot `dmg_nrects >= dmg_rect`; `dmg_nrects == 0` with `dmg_rect > 0` is
a never-wired counter, a hard error rather than a zero. A one-line `fbs_added` (`ADDFB2`
count) additionally kills the age hypothesis by observation rather than inference: ≤ ~8 for
a whole run confirms buffer reuse; climbing at the flip rate would mean per-frame
reallocation is real after all.

**Reference numbers from the landed run** (aarch64/HVF, 1280x800, 88 samples over 176 s,
70 s continuous motion at 60 injected moves/s): burst `flips/s = 8.16`,
`cursor_mv/s = 8.16`, `evpush/s = 174.63` (evdev emits `EV_REL` X, `EV_REL` Y and `EV_SYN`
per motion event, so 174.63/3 = 58.2 moves/s — the moves genuinely reached the guest ring),
damage 0 full / 571 rect / 0 skip. The sanity identity
`dmg_full + dmg_rect + dmg_skip == atomic` was exact in every window. No stale pixels:
consecutive screendumps differ by 126, 108 and 288 px, every one inside a ~15x21 px box in
the panel bar — the clock digits. Single run.

### 9. `kms_swrast` destroys imported handles with `MODE_DESTROY_DUMB`

Gallium's kms-dri winsys releases *every* `pipe_resource` it imported through
`DRM_IOCTL_MODE_DESTROY_DUMB` (`src/gallium/winsys/sw/kms-dri/kms_dri_sw_winsys.c:288-296`),
not through `GEM_CLOSE` — that is upstream's shape, not a bug in Mesa. Our
`std_handle_destroy_dumb` (`drivers/src/drm_device_interface.rs:2833`) takes **no
`open_id`** and consults `DUMB_BUFFERS` only, so a handle that was minted by an import is
not found and is never released. cosmic-comp imports as a matter of course, once per
composited frame, so this **leaks one object per frame** for the whole life of a session.

Today the leak is bounded by the fact that imports do not mint handles at all (item 7's
Stage 3 is not implemented), so the current cost is the lookup miss rather than unbounded
growth — but the fix belongs with the item 1 refcount work, because that is what makes
"release a handle" mean something: `DESTROY_DUMB` must gain `open_id` and route to the
same per-open unref path as `GEM_CLOSE`, dropping exactly one reference regardless of which
registry minted the handle. A counter on `[DRMSTAT]` (live objects, bounded over a 60 s
session) is the cheapest detector, and it is the same counter item 1's compositor gate
needs.

### 10. The `[DRM-SRV] mmap` trace is unconditional and floods a session

`servers/drm/src/lib.rs:211-219` prints `[DRM-SRV] mmap token=… map_info=0x0N -> {uncached,
writeback}` on **every** resolved mmap token, outside `pci::RENDER_DEBUG`, with a source
comment saying the unconditional print is deliberate — it was the only evidence that
`18a7a9f`'s cacheability scoping is by cache type rather than blanket. That evidence has
been collected. The line now costs a per-byte UART write on a path COSMIC takes
continuously: **146 lines in a ~7 minute session**, on the same console that shreds guest
output when it interleaves (instrument-reliability entry 6). Gate it — either behind
`RENDER_DEBUG` or behind a first-N-per-cache-type one-shot, which keeps the evidence
property at bounded cost. Cheap, and it makes every future session log more trustworthy.

### 11. Deferred work and known limitations

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
  The idle counters in item 8 confirm it from data. The separately-observed 128-dmabuf-fd
  burn in ~1 s and the `MAX_FDS` 64→128 raise are untouched by this and still stand.
- **llvmpipe** — the TCG-performance lever, staged but not landed. softpipe was chosen for
  correctness (portable C, no per-arch LLVM codegen bring-up ×2).
- **Synthetic sysfs** — the read-only `/sys/dev/char`, `/sys/class/drm`, `/sys/class/input`
  design in `docs/design/k4-drm-design.md` is execution-ready but deferred; no current
  consumer needs the enumeration.
- **DRM ioctl gaps cosmic-comp tolerates** (kernel returns Unsupported): `VRR_ENABLED`
  property, syncobj. Nothing optional is advertised in the property table on purpose —
  smithay guards each and degrades cleanly.
- **`FENCE_FD_IN`** (sync-file import) still needs the reverse plumbing and has no
  signalled-by-construction shortcut, unlike `FENCE_FD_OUT` (`a0325c6`). Real
  `DRM_IOCTL_SYNCOBJ_*` are not on the critical path — Mesa 25.3.6 compiles the SIMULATE
  path unconditionally. **A dependency to remember:** `a0325c6`'s out-fence eventfd is
  signalled at creation, which is correct **only while `VirtioGpu::submit` is a synchronous
  busy-spin**. If the ISR work ever makes submission asynchronous, that becomes a lie and
  must become a real waitable fence. The dependency is on `submit`, not on the syncobj
  code, and the source comment says so.
- **ELF loader follow-ups from the dynamic-linking wave**: interp is eagerly loaded
  (~4.8 MB per exec), and there is a pre-existing buddy-slack leak on the eager→lazy split.
- **`/proc/self/exe` returns `/bin/init`** regardless of the caller.
- **libseat shim eventfd workaround** (`0bed5ad`) is inert now that the kernel honours
  `EFD_NONBLOCK`, and can be simplified.
- **DRM page-flip event timestamps** (`drivers/src/drm_device_interface.rs:394,398-400`)
  are still built from the 100 Hz tick scheme, and smithay reads them for presentation
  feedback — worth moving to the interpolated clock in the same sweep.
- **Harness gotchas in `~/code/leandros-artifacts/m8_cursor.py`.** Two, both silent: it
  picks its "busiest window" by `curs_mv` delta, which is identically 0 on the legacy KMS
  path, so a legacy-path control prints a degenerate `1.00 flips/s` instead of erroring
  (key the window on `evpush` instead); and its positional regex zeroes every `[DRMSTAT]`
  field after `flip_us` now that `c5abb8d` inserted five `dmg_*` fields there. Prefer
  `m9-fb-damage-clips/m9_analyze.py`, which parses `key=0xHEX` pairs order-independently.
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
  as `leandros-applet`. It is a Stage 0 measurement instrument (it enumerates the
  `wl_registry` of every `wayland-*` socket in `$XDG_RUNTIME_DIR` and exits); nothing in
  the session depends on it. **Only aarch64 has been built** — x86_64 was never staged.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at exit
  144. Prefer `scripts/scmrun.py` (one process per command, explicit pre-send drain, fixed
  read window, no `expect()`) over `driver.py cmd` for anything whose number will be
  quoted, and open every boot with a positive control.
- When host tracing is on, always pass `-D <file>`. A trace stream sharing the guest's pty
  interleaves per character and silently destroys both `grep` results and harness sentinels
  (instrument-reliability entry 6).
