# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`18a7a9f`), after a wave that landed
two commits on this Mac and three more on the Linux box, and that turned four prepared
patches into measured results.

**What this wave established, patch by patch.** The blob-cacheability fix (former item
1) landed as `18a7a9f` here and as `0df1810` on the box: `vkrender`'s `s0_submit` passes
**3/3 under x86_64/KVM without `VN_PERF=no_fence_feedback`**, where it timed out 2/2, and
the scoping is proven on the wire rather than inferred — five host-visible blobs are
mapped in a session and **exactly one**, the `map_info=0x03` fence-feedback buffer, takes
the uncached path while the Venus ring and the other three stay write-back. The aarch64
half's load-bearing assumption became a runtime fact: `MAIR_EL1 before=0x…00ff
after=0x…4400ff`, so attributes 1..7 really were zero as delivered and index 2 was free
to claim. The former item 2 (`ATTR_NOCACHE` names index 3 "normal NC" and index 3 is
Device memory) was corrected in the same commit, with the framebuffer deliberately left
as Device memory and the reason written next to the flag; what survives is a smaller
comment defect, opened as item 6. The former item 3 (x86_64 has no PAT setup) had its
**premise refuted** — Limine 11.4.1 programs `IA32_PAT` itself and already puts WC at
PA5 — and is rewritten as item 5 around the pre-existing BSP/AP divergence that finding
exposed, with a prepared patch. The PRIME blob export (former item 5) landed on the box
as `e083202`: `venustest` **68 → 80/0** with all twelve phase-5 reports emitted and
`no host3d blob on this host` absent from every log, and the decisive downstream test is
positive — a purpose-built `vkswap` creates a **`VK_EXT_headless_surface` swapchain**,
acquires an image against a fence, transitions it and presents it, 21/0, with a negative
control on the reverted kernel failing at exactly `create_swapchain` and nowhere else.
That also closes the former item 7 (borrowed VMOs grown, leaked and truncated), whose
three guards were separately demonstrated live on this Mac by a throwaway dumb-path A/B:
3 FAIL → 3 PASS with the raw return values printed (`mmap` 1089753088 → -1, `write` 8 →
-1, `ftruncate` 0 → -1). Neither of those two commits is on this Mac yet, which is item
1. The `SIMULATE_SYNCOBJ` patch (item 2 below) was measured and **not** landed: the
kernel change is right and its premise is confirmed on the wire, but four of its eleven
new subtests are mis-specified and cannot pass anywhere — split out as item 3, with the
host-side gap they exposed as item 4. The `FB_DAMAGE_CLIPS` instrument landed as
`c5abb8d`, and a source-analysis pass over smithay `efeb597` **refuted the reading its
own commit message records**; item 9 is rewritten around the corrected finding, and the
Mesa-modifier bullet in item 12 loses its per-frame-reallocation claim as refuted. The
TIME_WAIT and AF_UNIX-`listen()` patches (items 10 and 11) were prepared and cross-
checked; item 11's landing was in flight when this was written.

**Two things were in flight at the time of writing and must be checked before they are
believed.** (a) `c5abb8d`'s `drmsmoke` 22/0 gate: the diagnostic run that produced its
numbers was terminated before `drmsmoke` and `idletest` ran, so the commit is in the
tree with its non-regression gate unclosed. 22/0 on both arches closes it; anything else
is a real regression in `present_damaged`, since `userland/drmsmoke/src/main.rs` contains
no assertion on `FLIPS_SUBMITTED` or `[DRMSTAT]` at all and therefore cannot be moved by
the counter redefinition. (b) Item 11's landing: `scmtest` **31/0** on both arches on
fresh images means it landed clean and the baseline moves to 31; 30/0 means the subtest
did not run and the number is meaningless, not a pass; 31 lines with one FAIL is a real
defect in the reservation path.

Earlier waves this session, compressed: `05f7279` (aarch64 kernel softfloat — six
separate items trace back to this clobber, directly or as the cause that retired them),
`531f21e` (harness prompt detection), `4085b7f` (nested epoll), `75b32e3` (sub-tick
`CLOCK_MONOTONIC`), `26eebf0` (AF_INET loopback), `05bb0fe` (evdev monotonic
timestamps), `77f170d` (memfd tmpfs-slot leak + TGID canonicalisation), `b2260b4`
(`run-qemu.sh --venus` + `vkrender` staging — the first GPU work ever submitted from
LeandrOS), `9be954f` (`import_fd` EMFILE double-release, use-after-free class),
`07d461c` (repeat `listen()`, dead `init-server` crate), `97a979e` (subtest comments stop
citing TODO item numbers), plus this wave's `c5abb8d` and `18a7a9f`. Thirteen commits on
this Mac; three more on the box (`0df1810`, `e083202`, `eccc4e9`). The item count does
not fall much across the session because analysis kept finding pre-existing defects that
were always there and simply unmeasured, not because work ran out.

---

## Standing context

Facts that future work depends on and should not have to re-derive.

**Goal.** Run the COSMIC desktop environment *unmodified* (source: `../cosmic-epoch`)
on both x86_64 and aarch64 under QEMU. No COSMIC source patches; build-configuration
flags (`--no-default-features`) are allowed. Everything beneath COSMIC — kernel, libc,
system libraries, daemons — is ours. **This constraint now has a load-bearing
consequence** (item 9): the primary-plane over-damage is inside `OutputDamageTracker`
and `DamageShaper`, so a real fix there is a COSMIC/smithay source change and is
therefore out of bounds. The reachable outcome is an upstream reproducer, not a patch.

**Where it stands.** The desktop runs on both arches: cosmic-session → cosmic-comp on
KMS/softpipe → busd → cosmic-bg + cosmic-panel renders a wallpaper plus a full-width
panel bar with an embedded Wayland client, clock ticking. Remaining desktop work is
quality and performance, not bring-up. Vulkan now runs **and presents**: `vkrender`
executes fill-buffer, compute and graphics work, and `vkswap` drives a headless-surface
swapchain to `vkQueuePresentKHR -> VK_SUCCESS`.

**Suite baselines.** On fresh images with `vfstest` run exactly once per image, both
arches: vfstest **36/0**, scmtest **30/0**, drmsmoke **22/0**, wakepolltest 10/0,
forktest 3/0, epolltest 9/0, polltest 6/0, sigtest 6/0, timertest 6/0, memtest 4/0,
idletest 2/0, evtest2 8/0. `waittest` is **5/0 or 3/2 on either arch** — a pure timing
race in `fork` → child `setpgid(0,0)`+`_exit` → parent `waitpid(-pid)`, measured on
pristine kernels too; either result is acceptable, on either arch, and the x86_64-vs-
aarch64 asymmetry seen in any single wave is noise. On a **Venus host** (the Linux box,
`--venus`): `venustest` **80/0** with the PRIME commit (68/0 without it), `vktest` 0
failures, `vkrender` **51/0** with `s2_checksum = 0x02C0FDC5` pinned across x86_64/KVM,
x86_64/TCG and aarch64/TCG, `vkswap` **21/0**. `vkrender` under KVM **no longer needs**
`VN_PERF=no_fence_feedback` — that dependency died with `18a7a9f`.

**A Mac `venustest` run is worth nothing, in either direction.** QEMU 11.0.2 on macOS
has **no blob-capable virtio-gpu device at all**: `virtio-gpu-pci,blob=on` is refused
with *"need rutabaga or udmabuf for blob resources"*, and neither `virtio-gpu-gl-pci`
nor any rutabaga variant is compiled in. `VIRTIO_GPU_F_RESOURCE_BLOB` is never
advertised, so no blob BO can be created, so nothing downstream of one can be exercised.
A Mac `venustest` reports **42 lines, 11 PASS / 31 FAIL**, byte-identical on patched and
unpatched kernels. Do not compare that against the box's 68/80 and conclude anything.
Everything blob-, HOST3D- or Venus-shaped goes to the box.

**Memory attributes, measured rather than assumed.**

- *aarch64.* `MAIR_EL1` arrives as a flat **`0x00000000000000ff`** under Limine 11.4.1
  — attribute 0 is `0xFF` (Normal WB/WA) and **attributes 1..7 are all zero**, i.e.
  Device-nGnRnE. `18a7a9f` installs **index 2 = `0x44`** (Normal Inner/Outer
  Non-cacheable) with a read-modify-write in `mmu::enable_identity`, before `arch::init`
  maps anything and before `smp_init` snapshots MAIR for the APs, and prints
  `[ARCH] MAIR_EL1 before=… after=…` once at boot so the inherited value stays visible.
  Index 3 (`ATTR_NOCACHE`) is Device-nGnRnE and always was; index 1 (`ATTR_DEV`) is too
  (item 6). The aarch64 framebuffer is therefore Device memory and is **deliberately
  left that way** — it works only because `pitch = width*4` keeps every access aligned.
- *x86_64.* Limine 11.4.1 **does** program `IA32_PAT`, to `0x0000_0105_0007_0406`
  (PA0 WB, PA1 WT, PA2 UC-, PA3 UC, **PA4 WP**, **PA5 WC**, PA6 UC, PA7 UC), decoded from
  a `mov ecx,0x277` / `wrmsr` site in `BOOTX64.EFI+0x42f34` guarded by
  `CPUID.01H:EDX.PAT`. `BOOTAA64.EFI` has zero such sites. Only our direct-boot path
  (`kernel/src/entry_x86_64.s`, which writes EFER and nothing else) leaves the reset PAT.
  `18a7a9f`'s commit message says the reset PAT applies; that is wrong on the Limine
  path, though it reaches the right conclusion anyway because PA2 is UC- in both tables.
  This is a static decode of a binary and has **not** been confirmed by a runtime read —
  item 5 adds the print that would.

**Instrument reliability — read this before trusting a number.** Five separate
instruments produced believable wrong numbers in a single day:

1. A screendump/serial parser keyed on field *position*: `m8_cursor.py`'s regex ran from
   `flip_us` onward, and `c5abb8d` inserted five `dmg_*` fields between `flip_us` and
   `curs_up`, so every field after the insertion point silently read **0** on a patched
   kernel. Parse `key=0xHEX` pairs order-independently (`m9_analyze.py` does).
2. A guard test that passed with its guard removed: `memfd_inflight_close` as first
   written could not fail, because the hazard window never opened. The same trap was
   walked into and *avoided* this wave — a `close(0)`-consequence check would have been
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
   lifetime came out 6 on HEAD and 7 patched — a clean +1 for one instance create/destroy
   pair. Running the same binary three times in one HEAD boot gave **6, 6, 7**. Venus
   notifies its ring opportunistically, so the count floats and the +1 was noise.

Two rules follow, and both are cheap. **Run a positive control**: send a known-failing
command (`nosuchbinary_xyz42`) as the first command of every boot and confirm the harness
reports it as failing. That single step catches 1, 3 and 4. **Prefer a structurally
distinctive observable over a count delta**: replacing the submit *count* with the submit
*payload size* settled the same question unambiguously (16 bytes occurs zero times in 72
submits across five HEAD lifetimes; exactly once per lifetime patched, always last before
`ctx_destroy`) — and it revealed the event inside a lifetime whose count was identical on
both kernels and had hidden it entirely. Cross-foot every count against a second,
independent source: the test binary's own `failures = N` trailer caught a `^\S+: PASS$`
extractor that reported `PASS=0` because the serial console emits CRLF.

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
  commit (`e083202`, box-only — item 1). It closes three hazards that were live and
  *measured* live, not theoretical: an unpatched kernel returned a valid mapped address
  for a page past the frames the DRM layer lent it, accepted an 8-byte `write()` into
  it, and *succeeded* at shrinking a borrowed frame list — order-0 frees out of an
  order-N buddy block. Until that commit reaches this Mac, the Mac tree still has all
  three.
- Release builds only — debug builds crash early. Test **both** arches in QEMU after
  every change. Minimum Limine revision is **6**, never downgrade.
- Regression images must be freshly regenerated — run vfstest **exactly once** per
  freshly generated image. A dirty f2fs image produces phantom failures
  (`chroot_confines_symlink_resolution`, `xattr_list_tmpfs`, `xattr_list_f2fs`). The
  historical aarch64 `xattr_list_f2fs` red did not appear anywhere in this wave, on
  either machine, consistent with it being that artifact and not an arch bug.
- **A guard test must be shown to fail with its guard removed, or it is certifying a
  hazard it never checked.** See the instrument-reliability entry above; a test that
  cannot fail and an instrument that cannot report failure are the same defect.
- **Subtest comments must not cite TODO item numbers.** Six did, and this file gets
  renumbered as items land — every citation had drifted within one day. Point to the
  defect or the commit instead; those don't move. The prepared `driverpy_venus.patch`
  currently violates this in a docstring ("see TODO.md item 4/12") and must be edited
  before it lands.

**Diagnostics in-tree** — flip to `true`, measure, flip back before committing:

| Flag | File | Measures |
|---|---|---|
| `DRM_STATS` | `drivers/src/drm_device_interface.rs:1344` | flips, cursor up/mv, atomic, atest, cplane, `dmg_{full,rect,skip,px}`, `blobs`, `evpush` |
| `CURSOR_DEBUG` | `drivers/src/virtio_gpu.rs:342` | cursor queue setup + selftest |
| `mm::gap2::ON` | `mm/src/gap2.rs:17` | memfd/MAP_SHARED path + frame checksum sampler |

**`DRM_STATS` is `true` at HEAD** — `c5abb8d` landed with it flipped on, so every boot
emits the periodic `[DRMSTAT]` line and harnesses have to filter it. That is a
housekeeping revert, not a design choice; see Housekeeping.

**`RUST_LOG=trace` cannot read smithay's own damage-tracking decisions.**
`cosmic-comp/Cargo.toml:61-62` sets `release_max_level_info` on `tracing`, so `trace!`
calls are compiled out of the release build and the feature ceiling cannot be raised
additively. Kernel-side counters are the only instrument, and the `FB_DAMAGE_CLIPS` blob
is the damage tracker's **verbatim** output (`PlaneDamageClips::from_damage`,
smithay `backend/drm/surface/mod.rs:68-100`, is a 1:1 `map` with no splitting or
merging), which is what makes the kernel-side decode a real measurement of a client-side
decision.

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
| 1 | This Mac and the Linux box have diverged — three commits to reconcile | Housekeeping — blocking | — |
| 2 | `SIMULATE_SYNCOBJ`: the kernel change is right; the patch is held on its tests | Bug — kernel | item 3 |
| 3 | Four phase-7 syncobj subtests are mis-specified and cannot pass anywhere | Bug — test | — |
| 4 | A `RING_IDX`-fenced submit against a ring-less context is silently dropped | Bug — kernel/host | — |
| 5 | x86_64 `IA32_PAT`: the BSP and the APs disagree — fix prepared | Bug — kernel | — |
| 6 | aarch64 `ATTR_DEV` is Device-nGnRnE, and a landed comment implies otherwise | Bug — comment | — |
| 7 | Vulkan presents through a headless swapchain; next is Wayland | Feature | item 8 |
| 8 | Cross-open dmabuf import is refused by design | Feature | — |
| 9 | Primary-plane over-damage is upstream; only a measurement remains | Perf | — |
| 10 | AF_UNIX `listen()` is lax in the opposite direction — fix prepared | Bug | a live COSMIC session |
| 11 | No TIME_WAIT — fix prepared, landing in flight | Bug | — |
| 12 | Deferred work and known limitations | Mixed | — |

---

## Prepared but not landed

Six patches are written and verified to `git apply --check` cleanly **against this Mac's
`18a7a9f`** (re-checked at reconciliation time, not inherited from an older base).

1. `~/code/leandros-artifacts/notes/m9-prime-export/prime_handle_to_fd_built_20260806.patch`
   — 4 files, +349/−27. Already **committed on the box** as `e083202` and verified there;
   this file is the route onto the Mac. See item 1.
2. `~/code/leandros-artifacts/notes/m9-simulate-syncobj/simulate_syncobj.patch` — 3
   files, +383/−8, built on both arches, run on the box. **Do not land as-is**: four of
   its eleven subtests are mis-specified (item 3). The kernel half is proven correct.
   See item 2.
3. `~/code/leandros-artifacts/notes/m9-x86-pat/pat_bringup.patch` — 331 lines, 3 files,
   all under `arch/x86_64/src/`. Builds all four kernel variants. **Never run.** Touches
   nothing any other lane owns. See item 5.
4. `~/code/leandros-artifacts/notes/m9-afunix-timewait/afunix_listen_strict.patch` —
   `servers/net/` + `userland/scmtest/`. Correct but deliberately held. See item 10.
5. `~/code/leandros-artifacts/notes/m9-afunix-timewait/tcp_time_wait.patch` — same two
   files. Landing was in flight when this was written; if `git log` shows it, delete this
   entry and item 11. See item 11.
6. `~/code/leandros-artifacts/notes/m9-damage-rootcause/damage_rect_dump.patch` — one
   file, `drivers/` only, entirely inside the `DRM_STATS` gate, **built** for both
   targets. Prints the decoded damage rect list. Optional; see item 9 for what it would
   and would not settle.

Also prepared, not a kernel patch:
`~/code/leandros-artifacts/notes/m9-driverpy-venus/driverpy_venus.patch` — teaches
`.claude/skills/run-leandros/driver.py` a `--venus` mode with the exact device line
`run-qemu.sh --venus` uses. Applies clean. **Edit out its TODO-item citation before
landing** (see the standing rule). See item 7.

---

### 1. This Mac and the Linux box have diverged — three commits to reconcile

Two `main`s, and **neither is a fast-forward of the other**:

| | this Mac | the Linux box (`forain@172.16.158.150:/home/forain/Projects/leandros`) |
|---|---|---|
| | `18a7a9f` drm: honour the host's requested blob cacheability | `eccc4e9` mkfs: stage vkswap when the venus artifact tree provides it |
| | `c5abb8d` drm: decode FB_DAMAGE_CLIPS and present only the damaged rects | `e083202` drm: export Venus blob handles through `PRIME_HANDLE_TO_FD` |
| | `a0f2c46` | `0df1810` drm: honour the host's requested blob cacheability |
| | | `a0f2c46` |

`18a7a9f` and `0df1810` are the **same change committed twice**, so a merge or rebase
will see the blob-cacheability hunks from both sides. `origin` is at `6a0eb0c` and
nothing has been pushed to it from either machine.

What each machine is missing, and why it matters:

- **This Mac lacks `e083202`.** That is not just a feature: it carries the borrowed-VMO
  immutability invariant, and the three hazards it closes were measured live on *this
  machine* (unpatched: a valid mapping one page past the lent frames, an accepted 8-byte
  write into it, and a successful shrink of a borrowed frame list). Practical route:
  apply `prime_handle_to_fd_built_20260806.patch`, verified to apply clean at `18a7a9f`.
  It also brings `venustest` 68 → 80 subtests, of which 12 need a Venus host to run.
- **This Mac lacks `eccc4e9`** (4 lines in `scripts/mkfs-f2fs-populated.py`, mirroring
  the existing `vkrender` block). Harmless without the binary — it stages `vkswap` only
  when the venus artifact tree provides it — and there is **no aarch64 `vkswap` binary
  anywhere**: the box has no arm64 binfmt handler, so the Alpine container cannot
  cross-build it. `zig cc -target aarch64-linux-musl` via `scripts/cc-aarch64-musl.sh`
  is the route if it is wanted.
- **The box lacks `c5abb8d`.** No Venus work depends on it, but the box is where any
  future damage measurement under KVM would run.

Also outside git: `vkswap.c` and `build-vkswap-alpine.sh` live at
`~/code/leandros-artifacts/notes/m9-vkswap/` (and in the box's venus-lane artifact
tree); the raw logs for the whole Venus wave are in `m9-lane-i-logs.tgz` there and under
`/tmp/m9lane/` on the box. Both pre-existing box stashes are still present and were
never popped — `stash@{0}` must still not be blind-popped (it would revert `4085b7f`).

### 2. `SIMULATE_SYNCOBJ`: the kernel change is right; the patch is held on its tests

`sim_syncobj_create` (`vn_renderer_virtgpu.c:145-190`) lazily submits, once per process,
an execbuffer with `size=0, command=0` plus `FENCE_FD_OUT`, and requires
`args.fence_fd >= 0`. We refuse it at `drivers/src/drm_device_interface.rs:3112`
(`exec.command == 0 || exec.size == 0`) and never write `fence_fd` back.

**The premise was corrected during design, and then confirmed on the wire.** The scary
consequence is not `close(0)`: `sim_submit` closes `args.fence_fd` only when
`batch->sync_count != 0`, which requires a `vn_renderer_sync` to exist, which requires
this probe to have succeeded — so today the `close(0)` is *unreachable, hidden by the
bug itself*. The live damage is quieter: `vn_ring_destroy`'s
`vn_renderer_submit_simple_sync` bails **before submitting** when the sync cannot be
created, so **every Venus instance leaks its host-side ring at teardown**. Measured: a
16-byte `SUBMIT_3D` occurs **zero times in 72 submits across five unpatched renderer
lifetimes**, and **exactly once per lifetime on the patched kernel, always the last
submission before `ctx_destroy`**, in both `vktest` and `vkrender`.

The trap this creates is the reason the patch is one indivisible change: accepting the
probe *without* also writing `fence_fd` on the real submit path would **arm** the
`close(0)` on every `vkDestroyInstance`. A partial fix is strictly worse than none. The
invariant is therefore established a layer up, in `kernel/src/syscall.rs::sys_ioctl` —
the one place with the caller's pid, address space and both the `drivers` and `vfs`
crates in scope, exactly where `PRIME_HANDLE_TO_FD` is already intercepted, so the
recorded "`fence_fd` blocked on fd plumbing" note is stale:

> Whenever the caller set `FENCE_FD_OUT`, `fence_fd` is written before we return — a
> real fd (`>= 3`) on success, **`-1` on every failure path**. It is never left holding
> the caller's incoming value.

A signalled `eventfd2(1)` is exact rather than approximate, because `VirtioGpu::submit`
busy-spins until the host retires the chain: by the time `EXECBUFFER` returns, the work
really is done. Mesa only ever `poll(POLLIN)`s, `F_DUPFD_CLOEXEC`s and `close()`s the
fd, which an eventfd satisfies. The accepted shape is narrow — `command == 0 && size ==
0` only; both half-zero shapes stay refused.

**Measured on the box (run C, x86_64/Venus, and run D stacked with PRIME on both
arches):** `venustest` reaches the predicted total of 79, but **79 with 4 FAIL**, not
79/0. Stacked with PRIME it is 91 total, 87/4, **identical subtest for subtest on both
arches**. `vktest` 0 failures, `vkrender` 51/0 with `s2_checksum = 0x02C0FDC5`,
`drmsmoke` 22/0. `close(0)` never happens and stdin survives — every command after
`venustest` in the same session ran and printed normally. The four failures are item 3
and are **not caused by this patch**: the stock kernel running the same binary produces
the identical single `[GPU] control-queue TIMEOUT, cmd=0x00000207` at the identical
subtest. All three branches of the invariant are nevertheless proven — fence-only
success (fd `>= 3`, pollable, dupable), failure (`-1`), and real-stream +
`FENCE_FD_OUT` succeeding, the last one via Mesa itself at every `vkDestroyInstance`.

**To land:** fix item 3, re-run `venustest` on the box, expect **79/0** alone or
**91/0** stacked on PRIME, then land. Nothing about the kernel hunks needs to change.

Still open afterwards: `FENCE_FD_IN` (sync-file import) needs the reverse plumbing and
has no signalled-by-construction shortcut; real `DRM_IOCTL_SYNCOBJ_*` are not on the
critical path since Mesa 25.3.6 compiles the SIMULATE path unconditionally. And the
eventfd is signalled at creation, which is **correct only while submission is a
synchronous busy-spin** — if the ISR work ever makes submission asynchronous this
becomes a lie and must become a real waitable fence. That dependency is on
`VirtioGpu::submit`, not on this patch, and the source comment says so.

### 3. Four phase-7 syncobj subtests are mis-specified and cannot pass anywhere

`phase7_submit_fence_fd_out_written`, `phase7_submit_fence_fd_signalled`,
`phase7_fence_fd_recycled_over_64_submits` and `phase7_failed_submit_releases_fence_fd`
set `VIRTGPU_EXECBUF_RING_IDX` over a **32-byte all-zero stream**. That makes the guest
set `VIRTIO_GPU_FLAG_INFO_RING_IDX` and QEMU route fence creation to
`virgl_renderer_context_create_fence(ctx, …, ring_idx, …)` — a per-context ring that a
zero stream never creates. The fence has nowhere to land, is dropped, and the guest
spins to its own timeout. The discriminator is inside the same run and needs no extra
experiment: the same context accepts a `flags = 0` submit seconds later
(`phase7_no_fence_fd_when_not_requested: PASS`), so nothing is wedged and `RING_IDX` is
the only variable.

They are unpassable **anywhere**: on a Venus host the ring does not exist, and on a
non-Venus host phase 7 is gated off entirely by `ctx_ok`.

**Fix:** drop `EXECBUF_RING_IDX` from those four real-submit subtests. Keep it in the
probe, which must stay byte-identical to Mesa's. `FENCE_FD_OUT` alone exercises the
whole fd path, and the fence then lands on the global timeline, which retires.

**Do not "fix" this in the kernel.** Refusing or rewriting `RING_IDX` would break Mesa,
whose submits all carry it — `vktest` and `vkrender` issue dozens per boot with zero
timeouts, because `vn_ring_create` really does create ring 0 first.

### 4. A `RING_IDX`-fenced submit against a ring-less context is silently dropped

Generalising item 3: a submission that asks the host to fence on a per-context ring the
guest never created is not refused — it is accepted, dropped, and costs the caller a
**full control-queue timeout** in `VirtioGpu::submit`'s busy-spin. Nothing in the tree
hits this today except the four subtests in item 3, and Mesa never can. It is recorded
because it is a denial-of-service shape for any future client and because a caller has
no way to tell it apart from a dead host. Whether the right answer is a guest-side
precondition check, a shorter timeout with a distinct error, or nothing at all is
**undecided** — this entry is a finding, not a plan.

### 5. x86_64 `IA32_PAT`: the BSP and the APs disagree — fix prepared

**The item this replaces had a false premise.** "There is no `IA32_PAT` bring-up in
`arch/x86_64/`, therefore the reset PAT applies" — the first clause is true, the second
is false on the boot path we actually use. Limine 11.4.1 programs `IA32_PAT` itself and
already puts **WC at PA5** (see Standing context for the decode). WC has been one PTE
bit away the whole time.

**The reason to act is a live cross-CPU divergence on `main` today, not throughput.**
`IA32_PAT` is per-logical-processor and an AP leaves INIT/SIPI with the **reset** PAT.
So on the Limine path the BSP runs `PA4=WP, PA5=WC, PA6=UC` while every AP runs
`PA4=WB, PA5=WT, PA6=UC-`. Limine's framebuffer mapping selects PA5. `arch::init` tries
to re-map the framebuffer `NO_CACHE`, but does so with `map_4k`, which returns `false`
the moment it meets one of Limine's huge pages — the loop says so in a comment and then
ignores the result. The console writes through Limine's HHDM mapping
(`drivers/src/framebuffer.rs:653`). If that re-map fails, **the console is WC on the BSP
and WT on every AP** — the same physical lines under two memory types on two processors,
which the SDM leaves undefined. It has not bitten us because WT is coherent and the
console is idempotent, but it is the shape of bug that appears as rare corruption rather
than a fault. **Inferred, not yet observed:** the "if that re-map fails" step is a code
reading, and check (b) below settles it in one boot.

**The prepared fix** (`pat_bringup.patch`, 331 lines, 3 files, builds all four kernel
variants, **never run**) makes every CPU agree: the BSP publishes its whole 64-bit PAT
and each AP writes it verbatim. `init_pat_bsp()` is the first statement of `arch::init`,
ahead of the GDT and of everything it maps; `init_pat_ap()` is the first statement of
`smp::sched_ap_entry`, at which point the AP has touched only its stack and parameter
block, both WB through PA0, a slot nothing changes. The slot is **PA5**, chosen so the
write is provably inert on the primary path: under Limine PA5 is already `0x01`, so the
read-modify-write of byte 5 is value-identical and cannot reinterpret a live translation
(including Limine's own PAT-bit framebuffer mapping); on the direct-boot path PA5 goes
WT → WC and provably has no users, since reaching PA4..PA7 requires the PAT bit and the
2 MiB PDEs `entry_x86_64.s:133` builds are `0x83` with bit 12 clear. PA1 (Linux's slot)
was rejected because it is selected by PWT alone and we inherit Limine's page tables
wholesale — we cannot grep a binary's PTEs for an inherited WT mapping.

**Verify in this order.** (a) The boot print `[ARCH] IA32_PAT before=… after=… wc=1`,
the direct analogue of the `MAIR_EL1` print, which converts the static decode above into
a runtime read. Expected `before=0x0000010500070406` unchanged on the Limine path,
`0x0007040600070406 → 0x0007010600070406` on direct boot; **any other `before` means the
safety case split must be re-checked before landing**, and `wc=0` means the CPU or
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
already-landed UC mapping in `18a7a9f`, which demonstrably works, so it is not a
regression introduced here.

Note also that WC is **weakly ordered where UC was not**. That moves us toward the
reference behaviour rather than away from it (the host explicitly asked for
`VIRTIO_GPU_MAP_CACHE_WC`, so Mesa's Venus path is written against WC semantics on native
Linux, and its ring submission goes through a locked atomic that drains the WC buffers),
but it is worth knowing if a blob ever gets a new consumer.

### 6. aarch64 `ATTR_DEV` is Device-nGnRnE, and a landed comment implies otherwise

The runtime `MAIR_EL1` read shows a flat `0x00000000000000ff`: Limine took the path that
writes `0xFF` flat, not the `0xFF | (dev_attr << 8)` one, so **attribute 1 is zero too**.
`PageDescFlags::ATTR_DEV` therefore selects Device-**nGnRnE**, not the Device-nGnRE the
flag has always been described as. Behaviourally harmless — nGnRnE is strictly stronger
and MMIO works — but two comments landed in `18a7a9f` are now known to be imprecise:
`arch/aarch64/src/paging.rs:19` still says "whatever the loader left" where the value is
now measured, and the `ATTR_NOCACHE` doc block at `:33` repeats the pre-measurement claim
that Limine writes `0xFF | (dev << 8)`. Correct both to what the register actually
contains. This is a comment fix with no code change; it is listed separately because the
same class of wrong comment (`ATTR_NOCACHE`, "index 3 (normal NC)") cost a full analysis
pass to discover and would have produced an alignment fault had the blob work reused it.

### 7. Vulkan presents through a headless swapchain; next is Wayland

**A Vulkan swapchain now exists on LeandrOS.** `vkswap` — a ~450-line dependency-free C
program in `vkrender`'s idiom (no Khronos loader; `dlopen("/usr/lib/libvulkan_virtio.so")`,
bootstrap from `vk_icdGetInstanceProcAddr`, device entry points via
`vkGetDeviceProcAddr`) — goes surface → present-capable queue family → caps/formats/
present modes → device with `VK_KHR_swapchain` → swapchain (256x256, 5 images) →
acquire against a real fence → a genuine `UNDEFINED → PRESENT_SRC_KHR` barrier submitted
on the queue and fence-waited → `vkQueuePresentKHR -> VK_SUCCESS`. **21 PASS / 0 FAIL**
on the box. The layout transition is deliberate: presenting an `UNDEFINED` image is
undefined behaviour, and a present that skipped it would be a spec violation that happens
to return `VK_SUCCESS`.

**The attribution is the cleanest in the wave.** The same binary on a kernel with the
PRIME commit reverted gives **16 PASS / 1 FAIL**, and the single failure is
`create_swapchain` (`vkCreateSwapchainKHR -> VkResult(-10)`). Everything that does not
need a shareable image — extension, surface, present support, caps, formats, present
modes, even `vkCreateDevice` with `VK_KHR_swapchain` — already worked. One binary, two
kernels, one moving part.

`vkrender` (`b2260b4`) still passes 3/3 subtests with `s2_checksum = 0x02C0FDC5` pinned
byte-identically across x86_64/KVM, x86_64/TCG and aarch64/TCG — set
`VKRENDER_EXPECT_CHECKSUM=0x02C0FDC5`, because the value is **printed but not asserted**
unless that variable is exported, and every comparison so far has been done by hand.

**Next.** `--present` (a dumb-buffer blit reusing `drmsmoke`'s `ADDFB2`/`SETCRTC`
sequence) is written and staged but unrun; it needs COSMIC stopped, since we never gate
`SETCRTC` on DRM master. After that, M4 is a Wayland client, still blocked on cross-open
dmabuf (item 8) — the headless swapchain does **not** need it, which is exactly why it
was the reachable milestone.

**`driver.py` Venus mode is prepared but unlanded** (`driverpy_venus.patch`). The gate
it was blocked on is answered: QMP `screendump` works under `-display egl-headless` but
only in its **bare** form — passing `device=<gl-dev-id>` fails with `"no surface"`, with
or without `head=0`, because the GL device has no surface. The patch refuses `--venus`
on non-UEFI boot modes and on macOS rather than degrading, on the principle that every
way of getting it wrong fails silently downstream. Edit out its TODO-item citation
before landing.

**Environment, still true.** The box is `forain@172.16.158.150`,
`/home/forain/Projects/leandros`, EndeavourOS (**Arch, not Debian**), virglrenderer
1.3.0, QEMU 11.0.1, Mesa 26.1.3, host GPU a Ryzen 9 7950X iGPU (RADV
RAPHAEL_MENDOCINO). aarch64 there needs `-cpu max,lpa2=off` (the Limine 11.4.1 FEAT_LPA2
wedge). macOS has no EGL and no blob-capable device at all — see Standing context. The
loader stays unshipped: the ICD exports only `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr`, so it
can never stand in for `libvulkan.so.1`.

**Build findings, load-bearing for anyone rebuilding these binaries.** `-std=c11` does
**not** compile against musl — strict ISO hides `clock_gettime`, `nanosleep` and
`CLOCK_MONOTONIC`; use `-std=gnu11`. Vulkan headers need `/usr/include/vk_video` as well
as `/usr/include/vulkan`, copied to a private dir — do not point `-I` at `/usr/include`,
it shadows the target libc's headers. The container recipe **cannot build aarch64 on the
box**: no docker, and podman pulls arm64 images but cannot execute them. Cross-compiling
with `zig cc` + `musl-dyn-link.sh` works, with two gotchas — zig cc enables UBSan by
default (link fails on `__ubsan_handle_*`, needs `-fno-sanitize=undefined`) and its
driver silently produces a **static** binary, which cannot `dlopen` the ICD. Corrected
recipes: `~/code/leandros-artifacts/notes/m9-m3-vulkan/build-vkrender-alpine-fixed.sh`,
`build-vkrender-aarch64-zig.sh`, and `m9-vkswap/build-vkswap-alpine.sh`.

### 8. Cross-open dmabuf import is refused by design

`open_may_reach` (`drivers/src/drm_device_interface.rs:1093`) deliberately scopes BOs to
their owning DRM open, which is correct for `b80ab5a`'s ownership model but blocks
`VK_KHR_display` and Wayland dmabuf, both of which import into a different open (and for
Wayland, a different process). Supporting them needs cross-open reachability with
host-resource refcounting across opens (`free_blob` today unconditionally unrefs and
releases the window span), `CTX_ATTACH_RESOURCE` for the importer's context,
`MAP_DUMB`/`ADDFB2` accepting blob handles, `SET_SCANOUT_BLOB` (absent), and the
connector's missing `DPMS` property. Several days, and deliberately not speculated into a
patch. This is the M4 gate; headless WSI does not need it, as item 7 demonstrates.

### 9. Primary-plane over-damage is upstream; only a measurement remains

**The kernel side is done and landed** (`c5abb8d`): `DrmDevice::present_damaged`
(`drivers/src/drm/device.rs:411`) copies just the sub-rectangles a `FB_DAMAGE_CLIPS` blob
names instead of scaling and flushing the whole surface, with rects clamped and
degenerate ones dropped rather than the commit rejected — a bad clip list is a hint we
are free to ignore, and failing there would stall the compositor. It also added
`DAMAGE_{FULL,RECT,SKIP,PX}` and `BLOBS_CREATED` to `[DRMSTAT]`. Judge it on the
kernel-side defect it fixes — that a skipped primary still cost a full-screen scale plus
full-screen `TRANSFER_TO_HOST` and `RESOURCE_FLUSH` — and **not** on flips/s, because
there is no perf headroom to recover. Its `drmsmoke` 22/0 gate was still outstanding at
reconciliation time (see the preamble).

**Two claims in `c5abb8d`'s commit message and in the diagnostic that produced it are
wrong, and are corrected here because `git log` cannot be edited.**

- The report said `dmg_skip = 0` "directly confirms" that we fail smithay's third skip
  condition (`age > 0 && last_state.old_damage.len() >= age`). **It does not follow.**
  `dmg_skip = 0` means the tracker never returned *empty* damage; that is independent of
  `age`.
- The headline "~7,800× over-damage" is a cumulative-window artifact and misleads. The
  per-interval data shows **damage tracking demonstrably works when idle**: exactly
  **40,960 px per present = 1280x32 = the panel bar**, for 86 continuous seconds. The
  age-0 fallback fires **twice, at bring-up (t ≈ 4 s), producing exactly 1,024,000 px**
  — its unmistakable fingerprint, since all three of smithay's "damage everything"
  branches push one rect equal to `output_geo` — and **never again in 176 s**.

**The age hypothesis is refuted on two independent lines.** From source: `Swapchain::
acquire` (`allocator/swapchain.rs:154-181`) calls `create_buffer` only inside
`if free_slot.buffer.is_none()` and no path drops a buffer on the way out; cosmic-comp
holds at most two slots (`QueueState::WaitingForVBlank` gates the next render), so the
steady state is a two-slot rotation with `age = 2` and `old_damage` at 2-3 entries
against `MAX_AGE = 4` — the condition is *satisfied*. The only resets are error arms that
`bail!` and produce no flip. From data: the burst value is **992,000 px, not 1,024,000**,
and no fallback branch can emit that number.

**What actually happens is `DamageShaper`, and the arithmetic is exact.** For a
full-output bbox at 1280x800 the shaper's tile grid is 4 x 8 = **32 tiles of 320x100**.
`992,000 = 31 x 32,000` — 31 of 32 tiles. `981,760 = 992,000 − 10,240`, and
`10,240 = 320 x 32` — one tile short by 32 rows at the tile column width. Equivalently:
full width and 767-775 of 800 rows. The idle rect (1280x32) is a `len() == 1`
passthrough with no shaping at all, which is why idle reads clean. The inflation is
therefore **specific to pointer motion**, and the mechanism is the shaper, not buffer age.

**It is not fixable from our side.** Every decision is inside `OutputDamageTracker` and
`DamageShaper`, in the compositor's address space, before a byte reaches the DRM
interface; there is no feedback path from the driver into the damage tracker, and the
shaper is unconditional (`damage/mod.rs:774`), not feature-gated, so
`--no-default-features` does not reach it. A real fix is a COSMIC/smithay source change,
which the standing goal forbids.

**What is on the primary plane, established rather than assumed.** Two elements: the
panel layer surface and the wallpaper layer surface (no windows, per the screendump).
The cursor is **not** among them — smithay puts a cursor-plane assignment in a separate
slot and only `overlay_plane_elements` are fed back as fake elements; the one path that
could push it onto the primary is the failed-`test_state_complete` reset, and `atest = 0`
for the entire burst (6 over the whole 176 s), so no test ever failed. `curs_up = 1` and
`curs_mv = 680 ≈ atomic = 684` confirm the plane is live.

**The one thing left worth knowing** is whether the pre-shaper damage set is genuinely
large or a handful of rects inflated into a million pixels. `damage_rect_dump.patch`
(built, unrun) prints `dmg_nrects` plus a bounded rect list and answers it in one run:
`n = 1` full-width at height 767-775 means the bbox shortcut (`shaper.rs:81-88`) and a
pre-shaper set of ≥2 rects one of which is ≥ 892,800 px; `n` in 4…32 with 320 px-wide
strips means the tiled path. If it turns out to be a few small rects inflated to 31
tiles, that is an **upstream smithay bug report with a reproducer** — a real outcome even
under a no-patch policy. Note the ceiling: the dump gives the shaper's *output*; the
*input* is not visible from the kernel at all and stays inferred, because
`release_max_level_info` compiles out the `trace!` calls that would show it.

**If that run happens, its own instrument must be checked first.** Assert that parsed
`r=` tuples equal `n` and that the `[/DMGRECTS]` sentinel is present; abort on mismatch.
Use the idle invariant already measured as the built-in positive control — over any 20 s
window with `evpush` delta 0, `dmg_nrects − dmg_rect` must be 0 and `dmg_px / dmg_rect`
exactly 40,960. If idle does not reproduce, the instrument is wrong and the motion
numbers must be thrown away. Cross-foot `dmg_nrects >= dmg_rect`; `dmg_nrects == 0` with
`dmg_rect > 0` is a never-wired counter, a hard error rather than a zero. A one-line
`fbs_added` (`ADDFB2` count) additionally kills the age hypothesis by observation rather
than inference: ≤ ~8 for a whole run confirms buffer reuse; climbing at the flip rate
would mean per-frame reallocation is real after all.

**Reference numbers from the landed run** (aarch64/HVF, 1280x800, 88 samples over 176 s,
70 s continuous motion at 60 injected moves/s): burst `flips/s = 8.16`,
`cursor_mv/s = 8.16`, `evpush/s = 174.63` (evdev emits `EV_REL` X, `EV_REL` Y and `EV_SYN`
per motion event, so 174.63/3 = 58.2 moves/s — the moves genuinely reached the guest
ring), damage 0 full / 571 rect / 0 skip. The sanity identity
`dmg_full + dmg_rect + dmg_skip == atomic` was exact in every window. No stale pixels:
consecutive screendumps differ by 126, 108 and 288 px, every one of them inside a
~15x21 px box in the panel bar — the clock digits. Single run.

### 10. AF_UNIX `listen()` is lax in the opposite direction — fix prepared

The AF_UNIX arm of `handle_listen` (`servers/net/src/lib.rs:1210`) is an unconditional
`ok_reply()`. Linux's `unix_listen()` has three gates, **in this order**: type not
STREAM/SEQPACKET → **EOPNOTSUPP (95)**, checked *before* the address; `u->addr == NULL`
→ EINVAL; `sk_state` neither `TCP_CLOSE` nor `TCP_LISTEN` → EINVAL. Note the asymmetry
with `inet_listen()`, which answers EINVAL for a DGRAM listen — the two must **not** be
made symmetric, and the AF_INET arm (`07d461c`) is left alone. There is no persistent
"connect in progress" state on Linux, so our `UnixPendingAccept` maps onto ESTABLISHED
and is EINVAL like `UnixConnected`; there is no fifth answer to give.

The prepared patch adds a subtest with **eight** assertions of which **five** must fail
against an unpatched kernel (unbound listen, socketpair end, connector, accepted socket,
DGRAM); the other three pass at HEAD and are explicitly **not** counted as evidence —
they exist so that "make AF_UNIX `listen()` always fail" and "re-arm the address on every
`listen()`" cannot pass. One falsifying mutation is worth naming because it is subtle:
moving the type gate *below* the state match makes the DGRAM case report errno 22 instead
of 95, which is exactly what a plausible-looking fix gets wrong.

**Held deliberately, and the reason is not the code.** The patch is a no-op for every
healthy server — a working AF_UNIX server is `UnixListening` at `listen()` time and that
arm still answers 0, idempotently — and every in-tree caller binds first and checks. But
the in-tree audit is not the population at risk. The risk is an out-of-tree component
(cosmic-comp, cosmic-panel, busd, tokio/zbus) whose `bind()` fails on a **dirty** image —
a stale `S_IFSOCK` under `/run/user/N` is not hypothetical here, `/data` survives reboots
— which today limps on as a zombie listener and after the patch exits at `listen()` and
gets restarted by `launch_pad` in a loop. That reads exactly like the crash-loop
signatures this project has spent whole waves chasing.

**To land:** `scmtest` on fresh images both arches (expect 31/0 alone, 32/0 with item
11), then a full COSMIC session on each arch checking for new `listen` EINVALs and
`launch_pad` churn, **then a second session against the image the first run left
behind** — the dirty run is the one that would expose a component relying on the zombie
behaviour, and a fresh-image-only validation would miss it.

### 11. No TIME_WAIT — fix prepared, landing in flight

`handle_close` calls `socket_set.remove()` immediately, so a closed TCP port can be
rebound at once where Linux would hold it in TIME_WAIT. The prepared patch models the
**port reservation, not the protocol**: a 60 s reservation (Linux's `TCP_TIMEWAIT_LEN`,
expressed as `60 * 100` ticks at 100 Hz), recorded only for the **active closer of a
connection that reached Established** (smoltcp's state at close decides; `CloseWait`/
`LastAck` are the passive close and do not park). The port is read from smoltcp's
`local_endpoint()`, not `bound_port` — `handle_accept` leaves `bound_port == 0`, and the
accepted socket sharing the listener's port is precisely what makes a restarted server
fail to rebind on Linux.

**`SO_REUSEADDR` had to become real in the same patch**, and this is the half a reviewer
should look at deliberately: `NET_SETSOCKOPT` was a bare `ok_reply()` that did not even
forward its arguments, so without recording the flag this change would make server
restarts *worse* than the divergence it fixes. `TIME_WAIT` is a strict leaf lock —
snapshotted before `SOCK_TABLES` is taken, recorded only after both it and the stack lock
are released — and a full 64-slot table **fails open**, so behaviour under pressure
degrades to today's behaviour rather than to a bind that cannot be satisfied. The
`sock_type == SOCK_STREAM` guard is load-bearing, not cosmetic:
`SocketSet::get::<tcp::Socket>` panics on a type mismatch.

Deliberately omitted: no lingering socket and no TCP TIME-WAIT protocol state (nothing
absorbs a late duplicate segment); no conflict check against *live* bound ports, so a
dead-but-recent port is refused while a live one is not — an asymmetry that is deliberate
and commented in the source; no `SO_LINGER`/`SO_REUSEPORT`/tunables; nothing for UDP.

Five assertions, of which **one** (`b`: rebinding a just-closed accepted socket's port
must give EADDRINUSE) must fail unpatched — stated plainly because it is the honest
number. Note that assertion (d) is a shape guard and not a detector: removing the
reserved-port skip from `alloc_ephemeral_port` makes it fail only ~1/28232 of the time.

**Landing was in flight when this was written.** Expect `scmtest` **31/0** on fresh
images, both arches. If it landed, delete this item.

### 12. Deferred work and known limitations

- **Doom does not link relibc.** `../doomgeneric/Makefile.leandros` links
  `userland/target/<arch>-unknown-none/release/libleandros_libc.a`, whose allocator is
  `userland/libc/src/mem.rs` — a ~20-line **bump allocator over `brk(2)`** with no free
  list, no dlmalloc and no `mmap` path. The retired malloc-hang item had blamed relibc;
  it could never have been right. Worth stating plainly so the next person debugging a
  Doom allocation does not start there. doomgeneric's zone default is 4 MiB
  (`DEFAULT_RAM 4`); the 16 MiB case is reachable only via `-mb 16` and also passes.
- **Mesa modifier support.** The claim that our GBM lacking
  `gbm_bo_create_with_modifiers2` makes smithay reallocate the swapchain per frame is
  **refuted**: `allocator/swapchain.rs:154-181` caches slots and allocates only when
  `buffer.is_none()`, `allocator/gbm.rs:200-238` has a documented Invalid/Linear
  fallback, and an allocation failure would surface as `FrameError::Allocator` — no flip
  at all, not a degraded one. The idle counters in item 9 confirm it from data. The
  separately-observed 128-dmabuf-fd burn in ~1 s and the `MAX_FDS` 64→128 raise are
  untouched by this and still stand. Revisit with PRIME/linux-dmabuf.
- **llvmpipe** — the TCG-performance lever, staged but not landed. softpipe was chosen
  for correctness (portable C, no per-arch LLVM codegen bring-up ×2).
- **Synthetic sysfs** — the read-only `/sys/dev/char`, `/sys/class/drm`,
  `/sys/class/input` design in `docs/design/k4-drm-design.md` is execution-ready but
  deferred; no current consumer needs the enumeration.
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
- **Harness gotchas in `~/code/leandros-artifacts/m8_cursor.py`.** Two, both silent:
  it picks its "busiest window" by `curs_mv` delta, which is identically 0 on the legacy
  KMS path, so a legacy-path control prints a degenerate `1.00 flips/s` instead of
  erroring (key the window on `evpush` instead); and its positional regex zeroes every
  `[DRMSTAT]` field after `flip_us` now that `c5abb8d` inserted five `dmg_*` fields
  there. Prefer `m9-fb-damage-clips/m9_analyze.py`, which parses `key=0xHEX` pairs
  order-independently.
- **Build gotcha:** building a userland test binary with a bare `cargo build` instead of
  `scripts/build-userland.sh` omits `-C relocation-model=static`, producing a PIE whose
  `.data.rel.ro` our loader never relocates. It then faults at `__libc_start_main+0x44`
  with `CR2=0`, before `main` — a distinctive signature whose cause is not obvious from
  the fault alone. Always build userland through `scripts/build-userland.sh`.

---

## Housekeeping

- **`DRM_STATS` is `true` at HEAD** (`drivers/src/drm_device_interface.rs:1344`).
  `c5abb8d` landed with the diagnostic flipped on, against the standing "flip back
  before committing" rule, so every boot emits the periodic `[DRMSTAT]` line and every
  harness has to filter it. Flip it back to `false` — and note that any measurement run
  since `c5abb8d` was taken with it on, which is why it was not simply reverted blind.
- **Fresh-worktree gotcha: the guest boots with no shell.** `build-all.sh` and
  `mkfs-f2fs-populated.py` resolve the sibling repos as `$ROOT_DIR/../<repo>`, and an
  agent worktree's parent is `.claude/worktrees/`, not `~/code/`. The build **exits 0**
  and only prints `⚠️ brush source not found … skipping`; the failure appears at runtime
  as `login: exec failed` / `session ended, restarting login`, i.e. `/bin/login`
  execve()ing a shell that is not in the image. `brush`, `coreutils` and
  `bottom-leandros` symlinks were added under `.claude/worktrees/` alongside the
  pre-existing `doomgeneric`, `mame` and `relibc`, and are left there deliberately.
- Untracked disk-image backups and `ports/*/.work/` remain gitignored
  (`f2fs-data0-*.img.*`, `ports/*/.work/`).
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at
  exit 144. Prefer `scripts/scmrun.py` (one process per command, explicit pre-send
  drain, fixed read window, no `expect()`) over `driver.py cmd` for anything whose
  number will be quoted, and open every boot with a positive control.
