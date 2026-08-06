# LeandrOS — TODO

Single source of truth for remaining and future work. Anything finished is deleted
from this file, not marked done — `git log` is the record of what happened.

Last reconciled against `main` on **2026-08-06** (`b2260b4`); item 4 and the item 10
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
down to 4-11 and the former item 10 (deferred work) is now item 12. Same day, a seventh
wave closed the former items 4 and 5 (the memfd tmpfs-slot leak and the
`tmpfile_owner_of` TGID canonicalisation), both landed together in `77f170d` and
verified on both arches: `memfd_anonymous_reclaim` 300/300, `memfd_inflight_close`
passing (only after a flaw in the test itself was fixed — see the guard-test lesson in
Standing context), `scmtest` 26/0 → 28/0, and a clean aarch64 A/B against pristine
`420adf7` showing the `smithay-clipboard` OutOfMemory panic present on the old kernel
and absent on the new one. A new item was split out of that verification for a
pre-existing, unrelated defect the double-release audit surfaced: `import_fd`
double-releases on EMFILE. Former items 6-12 shifted down to 5-11. Same day, an eighth
wave escalated the former item 4: a source-analysis pass over both EMFILE arms and
`release_vnode`'s five fd kinds found the double release is not cosmetic — it lands on
a live reference every time (the sender's fd is never revoked by `export_fd`) and
three of five kinds free live kernel state out from under an open fd
(`DynamicDevice` open-id reuse, `EventFd`/`TimerFd` free-slot-sentinel aliasing),
making it use-after-free class rather than a leak. Retitled, given a prepared fix and
regression subtest (`~/code/leandros-artifacts/notes/m9-import-fd-emfile/import_fd_emfile.patch`,
`scmtest` 28/0 → 29/0), and promoted to item 1 ahead of the Venus items. Former items
1-3 shifted down to 2-4; items 5-11 unchanged. Same day, a ninth wave landed the M3
`vkrender` milestone at `b2260b4`: `vkrender` executes real GPU work for the first
time — fill-buffer, compute and graphics subtests all pass, with `s2_checksum =
0x02C0FDC5` pinned identically across x86_64/KVM, x86_64/TCG and aarch64/TCG — and
`run-qemu.sh --venus` landed and reproduces `venustest` 68/68 and `vktest` 0 failures on
both arches. The former item 2 (Venus/M3) is rewritten and retitled around the
milestone. The former item 4 (`driver.py` has no GL path) is resolved and deleted: QMP
`screendump` under `-display egl-headless` works in its bare form (no `device=`), so a
`driver.py` Venus mode is unblocked; the finding is folded into the Venus item since it
also gates M3's presentation step. Chasing an `x86_64/KVM`-only `vkrender` `s0_submit`
timeout (TCG passes on both arches) found a real kernel defect — blob mappings ignore
the host's requested cacheability, forcing write-back on memory Mesa's fence-feedback
path asked to be write-combined — split out as a new item at position 2, right after
`import_fd`. Former items 2-3 shifted down to 3-4; former item 4 deleted; items 5-11
unchanged. Same day, a tenth wave completed the analysis on item 2: `git log -S` on the
warning string traced the cacheable override to a deliberate deferred-scope decision
recorded in `0dfc362`, not a workaround, and both reasons given there are now dead. A
source pass over both arches found the fix needs a new, arch-neutral `WRITECOMBINE`
flag rather than reusing `NOCACHE` — x86_64 has the attribute (PCD, no PAT needed since
there is no PAT setup to make UC anything but the reset state) but aarch64's
`ATTR_NOCACHE` produces Device memory, not Normal-NC, because neither Limine nor our
direct-boot path programs MAIR attributes 2..7. Item 2 is rewritten around the completed
analysis and a prepared patch, confirmed to stack with the in-flight primary-plane work
in both orders. Two new items were split out of what the pass uncovered along the way:
item 3 (the `ATTR_NOCACHE`/MAIR gap is live independent of the blob work — it also means
the framebuffer has silently been Device memory all along) and item 4 (x86_64 has no PAT
or MTRR setup, so true write-combining is unreachable there either, a separate ceiling
worth recording). Former items 3-11 shifted down to 5-13. Same day, an eleventh wave
completed the analysis on item 6 (`PRIME_HANDLE_TO_FD` rejects Venus blob handles): a
source pass over the kernel's blob/dumb-buffer registries, the borrowed-VMO lifecycle,
and Mesa's WSI import paths (`wsi_common_drm.c`, `vn_renderer_virtgpu.c`) produced a
prepared patch and a retitle to match. Three new items were split out of what the pass
uncovered along the way: a `SIMULATE_SYNCOBJ` gap where a rejected zero-size execbuffer
leaves `fence_fd` unwritten and Mesa then `close()`s stdin; the borrowed-VMO
grow/leak/truncate hazards audited while designing the export, closed by one stated
invariant the patch enforces; and the cross-open dmabuf gap that PRIME export alone does
not close, needed for `VK_KHR_display` and Wayland but not for headless WSI. Former
items 7-13 shifted down to 10-16.

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
28/0 (the 26th subtest, `inet_loopback_tcp`, added in `26eebf0`; the 27th and 28th,
`memfd_anonymous_reclaim` and `memfd_inflight_close`, added in `77f170d`), wakepolltest
10/0, forktest 3/0, epolltest 9/0 (the 9th subtest, `nested_epoll`, added in `4085b7f`),
polltest 6/0, sigtest 6/0, timertest 6/0 (the 6th subtest, `clock_monotonic_subtick`,
added in `75b32e3`), memtest 4/0, waittest 5/0 — all on x86_64. On aarch64, waittest
also came out 5/0 in this run rather than the previously recorded 3/2; the
`wait_on_process_group` flake simply did not fire this time, so treat either result as
acceptable. This wave (`05bb0fe`) also ran `idletest` 2/0 and `evtest2` 8/0 green on
both arches; neither was previously listed in this baseline. **Also as of `b2260b4`:**
`vkrender` passes 3/3 subtests with 0 failures and 0 skips on x86_64/TCG and
aarch64/TCG, with `s2_checksum = 0x02C0FDC5`; under x86_64/KVM it needs
`VN_PERF=no_fence_feedback` until the blob-cacheability item (item 2) is fixed.
**Refined 2026-08-06
(`77f170d`):** the flake is not aarch64-specific — an A/B on a pristine `420adf7` kernel
gave 5/0, 3/2, 5/0 across three runs, and the patched kernel gave 5/0 x3 and 3/2 x8
across 11 runs, both on aarch64. It is a pure timing race in `fork` -> child
`setpgid(0,0)` + `_exit` -> parent `waitpid(-pid)`, with no tmpfs, memfd or SCM
involvement; either result is acceptable on either arch.

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
- **A guard test must be shown to fail with its guard removed, or it is certifying a
  hazard it never checked.** `memfd_inflight_close` (`77f170d`) as first written could
  not fail: with `tmp_inflight_inc` removed from `export_fd` it still passed, because
  the parent created the memfd and `fork()`ed before closing it, so the child inherited
  a copy and the parent's `close()` was never the last fd-table reference — the
  hazard window never opened. Fixed by having the child close its copy before blocking
  on the sync byte, after which removing the guard fails deterministically (`child
  status=256`, pattern mismatch). Verify every guard test this way before trusting it.

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
| 1 | `import_fd` double-releases on EMFILE — use-after-free class | Bug — kernel | — |
| 2 | Blob mappings ignore the host's requested cacheability — fix prepared | Bug — kernel | — |
| 3 | `ATTR_NOCACHE` on aarch64 is Device memory, not Normal-NC | Bug — kernel | — |
| 4 | x86_64 has no PAT or MTRR setup | Bug | — |
| 5 | Vulkan renders on LeandrOS; next is presenting it | Feature | — |
| 6 | PRIME export for blob handles — fix prepared (headless WSI unblocked) | Bug — kernel | — |
| 7 | `SIMULATE_SYNCOBJ`: we reject the probe, and Mesa then closes stdin | Bug — kernel | — |
| 8 | Borrowed VMOs can be grown, leaked and truncated | Bug — kernel | — |
| 9 | Cross-open dmabuf import is refused by design | Feature | — |
| 10 | Primary-plane recomposite (FB_DAMAGE_CLIPS is the instrument, not the fix) | Perf | — |
| 11 | `listen()` twice returns EINVAL — fix prepared | Bug | — |
| 12 | `unused variable: port` in `handle_close` — not a leak, fix prepared | Cleanup | — |
| 13 | Delete the unreachable `init-server` crate | Cleanup | — |
| 14 | AF_UNIX `listen()` is lax in the opposite direction | Bug | — |
| 15 | No TIME_WAIT — ports are instantly reusable | Bug | — |
| 16 | Deferred / known limitations | Mixed | — |

---

### 1. `import_fd` double-releases on EMFILE — use-after-free class

**Confirmed at the source.** Both EMFILE arms of `import_fd`
(`servers/vfs/src/lib.rs:3722` and `:3725`) run `tmp_inflight_dec(&kind);
release_vnode(kind, pid); return -24;` — byte-for-byte what `drop_transfer` does
(`:3757-3758`). Meanwhile `servers/net/src/lib.rs:2144` sets `fit = i` (not `i+1`), so
the overflow loop at `:2151` re-drops `fds[i]`. One `export_fd` (`:3704`
`pipe_ref_inc`, `:3710` `tmp_inflight_inc`) is balanced by **two** releases.
`import_fd` has exactly one caller repo-wide.

**Two corrections to the earlier description.** `export_fd` does *not* lift the entry
out of the sender's table (`:3694-3711` only copies `kind`/`flags`), so the sender
keeps its fd open and **the extra release always lands on a live reference**. And it
needs no large batch: with `nfds == 1`, `fit` is 1, the import fails at `i = 0`, and
`for j in 0..1` re-drops that one descriptor.

**Severity, inferred from the release paths: three of five kinds are use-after-free
class.** `release_vnode` (`:3789-3817`) is not saturating in any way that helps — only
the counters saturate, and saturating at 0 is the corruption.
- **`DynamicDevice` — worst.** `device_close` twice takes refs 2→1→0, sets
  `DEVICE_OPEN_CLOSING`, sends `VFS_CLOSE` to the device server and frees the slot
  (`:1329-1346`). The sender's still-open fd then names an `open_id` whose server-side
  state is destroyed, and `device_open_alloc` (`:1299`) hands that id to the next open
  of *any* dynamic device — cross-process open-id aliasing, with the stale fd's ioctls
  landing on someone else's open. This is the DRM render node / dmabuf / evdev path,
  exactly what a Wayland session passes.
- **`EventFd` — severe.** refs 2→1→0 sets `EVENTFD_COUNTERS[slot] = u64::MAX`
  (`:3799-3804`), which **is the free-slot sentinel**: `handle_eventfd:4642` allocates
  via `position(|&v| v == u64::MAX)`, so with lower slots in use it deterministically
  re-hands out this one. Two unrelated processes then share a counter — verbatim the
  calloop aliasing bug the comment at `:3796-3798` exists to prevent.
- **`TimerFd` — severe**, same shape (`:3806-3809`).
- **`Pipe` — severe.** `writers`/`readers` saturating_sub twice (`:1060-1074`) drops
  the count to 0 under a live fd, so the peer sees spurious EOF/POLLHUP/EPIPE; if both
  sides reach 0 the ring resets and `handle_pipe:3576` reallocates the slot, after
  which the surviving fd's close decrements an unrelated pipe.
- **`TmpFile` — moderate, conditional.** The second `tmp_release_ephemeral` no-ops on
  its `in_use` guard, but the second `tmp_inflight_dec` does not: with another SCM
  transfer of the same slot in flight, `TMP_INFLIGHT[idx]` goes 2→0 and that other
  transfer loses its protection, reopening the use-after-free `77f170d` just closed.
- `MountedFile` and the rest: only `release_locks`; cosmetic.

**Collateral defect in the same two lines:** `release_vnode(tf.kind, pid)` passes the
**receiver's** pid, so `release_locks` (`:3920`) drops advisory locks the receiver
holds on that vnode through unrelated fds of its own — for a descriptor it never took
delivery of. `drop_transfer` correctly passes 0. The fix removes this for free.

**Reachable in normal operation.** `MAX_FDS = 128` with `alloc_fd` skipping 0-2 gives
125 usable, and the trigger is only "the receiver's table is full at the instant any
SCM_RIGHTS fd arrives" — one fd suffices. The deferred-limitations item already
records **128 dmabuf fds burned in ~1 s**, which is cosmic-comp at the ceiling while
Mesa and clients keep passing fds; the kind arriving in that window is
`DynamicDevice` or `TmpFile`, the two worst rows. It is not triggerable on demand
today only because nothing measures it. **`scmtest`'s `queued_fd_cap` does not and
cannot cover this** — it is purely sender-side (`userland/scmtest/src/main.rs:1639-1664`),
loops `send_fd_and_byte` and never calls `recvmsg`, so `import_fd` is never reached.

**Fix prepared** at
`~/code/leandros-artifacts/notes/m9-import-fd-emfile/import_fd_emfile.patch` (122
insertions, 5 deletions, 3 files; `git apply --check`-clean and round-trip verified at
`b7fb326`, and confirmed to stack with `m9-small-fixes/small_fixes.patch` in **either**
order with identical resulting trees). **`import_fd` now releases nothing on
failure**, making the caller sole owner of cleanup, so the rule states in one
sentence: *a `TransferFd` is consumed only on success; exactly one of `import_fd`
returning an fd or `drop_transfer` balances each `export_fd`, and on a negative return
the descriptor is still the caller's to retire.* That rule is written into
`import_fd`'s doc comment, with a matching note at `servers/net/src/lib.rs:2144`
explaining that `fit = i` is deliberate and must not be "corrected" to `i + 1`. The
alternative — having the caller skip index *i* — was rejected because it leaves "who
owns the failed one" a fact you must read two files to learn. Code delta is 2 lines;
the rest is comments and the subtest.

**Regression subtest** `scm_import_emfile_single_release`, single process, no fork:
`pipe2(O_NONBLOCK)`, read the empty ring as a control (must be `-1/EAGAIN`, proving
the final assertion can discriminate at all), send the *write* end over a socketpair
so `writers = 2`, `dup()` until EMFILE, `recvmsg`, close the dups, then re-read the
still-empty pipe. The process never closed `wr`, so it must still be `-1/EAGAIN`; the
double release drives `writers` to 0 and it reads `0` (EOF). Socket fds live above
`SOCK_FD_BASE` and are not in the fd table, so the socketpair survives exhaustion.
**Proving it fails without the fix is free — HEAD is the backed-out state**, so
building `scmtest` from the patch against an unpatched kernel must FAIL and against
the patched kernel must PASS. Failing signature: `[emfile] post ret=0 errno=0` where
`pre` read `-1/11`; if *both* read `0/0` the test is broken rather than the kernel.
**`scmtest` 28/0 → 29/0** (30/0 if the small-fixes patch also lands; the two are
additive).

### 2. Blob mappings ignore the host's requested cacheability — fix prepared

`drivers/src/drm_device_interface.rs:3740-3748` logs `"[DRM] WARNING: host asked for
non-cached blob mapping; mapping cacheable anyway"` and overrides the request.
`vkrender`'s `s0_submit` **times out under x86_64/KVM** (2/2 runs) while passing under
x86_64/TCG and aarch64/TCG, and passing under KVM with `VN_PERF=no_fence_feedback`.
Measured, not guessed: host tracing showed 20 submits total, all `size 24`, every one
fence-responded, and **zero submits during the 20 s wait** — the guest was spinning on
memory, not on an ioctl. Mesa explains why: with fence feedback (the default)
`vn_GetFenceStatus` reads `*slot->status`, a plain memory read that never touches the
ring (`vn_queue.c:1694`, `vn_feedback.h:103`), and `vn_feedback_buffer_create` picks the
**first** `HOST_COHERENT` memory type (`vn_feedback.c:76`) — memtype 2, which lacks
`HOST_CACHED` — so the host requests a non-cached mapping on exactly that resource
(`map_info=0x03`, WC). Subtest 0's own readback buffer is memtype 5 (`HOST_CACHED`,
`map_info=0x01`), mapped correctly, and reads back perfectly.

**The override was deferred scope, not a workaround.** `git log -S` on the warning
string gives one commit, `0dfc362`, whose message says so outright: mappings are
cacheable regardless of `map_info`, which was right for the Venus ring (the renderer
reports `CACHE_CACHED`), and honouring anything else would need a cache type plumbed
through the mmap reply. Both reasons are now dead — the 0x1007 reply carried one `u64`
and slot 1 was free (`VFS_POLL` already uses slot 1 for `seq`, so there is precedent),
and "no blob asks for anything else" stopped being true the moment Mesa's
fence-feedback buffers appeared. `0dfc362` also records that `RESOURCE_MAP_BLOB` had
never been sent on the wire at that point, so nothing was measured. Nothing is being
worked around; there is no reason to keep it.

**The arches are not symmetric, and that set the shape of the fix.**
- *x86_64: the attribute exists.* `PageFlags::NOCACHE` maps to PCD
  (`arch/x86_64/src/paging.rs:387`). There is **no PAT and no MTRR code anywhere in
  `arch/x86_64/`**, so the reset `IA32_PAT` applies and PCD alone selects UC. Real WC is
  not reachable without `IA32_PAT` bring-up, which we deliberately do not do — UC is a
  strictly stronger substitute with the same coherence guarantee and worse write
  throughput, and UC/WC aliases of one page are compatible where UC/WB and WC/WB are the
  SDM-undefined combinations.
- *aarch64: the attribute does not exist, and the code claims it does.*
  `arch/aarch64/src/paging.rs:21` declares `ATTR_NOCACHE = 3 << 2; // index 3 (normal
  NC)`. **That comment is false in practice.** The kernel never programs MAIR on the
  Limine path, and disassembly of the shipped `BOOTAA64.EFI` (Limine 11.4.1) shows MAIR
  built as `0xFF | (dev_attr << 8)` at file offset `0x209ec`-`0x209f0`, with a second
  path setting `0xFF` flat — **attributes 2..7 are zero on both**. Our own direct-boot
  path (`kernel/src/entry_aarch64.s:171`) writes `MAIR = 0x04FF`, likewise zero above
  index 1. A zero MAIR attribute byte is **Device-nGnRnE**, so `PageFlags::NOCACHE` on
  aarch64 produces Device memory, which forbids unaligned access — unusable for a buffer
  Mesa memcpys through, and it would have turned the KVM hang into an alignment fault.

**The fix** adds an arch-neutral `PageFlags::WRITECOMBINE` (bit 6), deliberately
**separate** from `NOCACHE`, mapping to PCD on x86_64 and to a newly-installed **MAIR
index 2 = `0x44`** (Normal Inner/Outer Non-cacheable, Linux's `MT_NORMAL_NC`) on
aarch64. Index 2 is the safe slot: its flag `ATTR_STRICT` had **zero users** in the
tree, so no live translation is reinterpreted. The MAIR write is a read-modify-write in
`mmu::enable_identity` preserving attrs 0 and 1, placed before `arch::init` maps
anything and before `smp_init` snapshots MAIR for the APs.

Scoping is by `map_info`, and only host-visible blobs have one — `blob_map_cache_type`
matches only entries with `map_phys != 0`, so dumb buffers and guest-backed blobs are
untouched. That matters, because the kernel *does* memcpy through those via
`phys_to_virt`. The Venus command ring reports `CACHE_CACHED` and stays write-back, as
does subtest 0's readback buffer (memtype 5, `map_info=0x01`), so `s2_checksum` has no
reason to move. Nothing is refused: refusing a non-cached MAP would break Venus outright
on both arches, a regression rather than an honest refusal.

**Patch prepared** at
`~/code/leandros-artifacts/notes/m9-blob-cacheability/blob_cacheability.patch` (373
lines, 7 files, +211/−21), `git apply --check`-clean and round-trip verified at
`1c5c708`, and confirmed to stack with the in-flight primary-plane work in **both**
orders (verified empirically by reconstructing that tree, not by inspection — their
hunks end at HEAD line 2487, mine are at 850, 1142, 3459 and 3601).

**Verify in this order.** First, the cheap local one: a one-line boot print of
`MAIR_EL1` either side of the new read-modify-write under aarch64/HVF. The "attrs 2..7
are zero" claim is static disassembly, not a runtime read, and **the entire aarch64
half rests on it**. Then, on the Linux box (the Mac has no EGL), the decisive test:
`run-qemu.sh --venus` x86_64 under **KVM** with `vkrender` and **without**
`VN_PERF=no_fence_feedback` — `s0_submit` must pass where it currently times out 2/2;
run it at least three times. Serial must show the non-cached mapping honoured for the
feedback blob (`map_info=0x03`) and **not** for the ring (`0x01`) — that line proves the
scoping, not merely that the hang went away. Non-regression: `s2_checksum` stays
`0x02C0FDC5` on all three configurations, `venustest` 68/68, `vktest` 0 failures, full
suite at baseline on fresh images.

**Residual risk worth naming, and its discriminator.** The root cause — a guest
write-back alias of a host WC mapping, with TCG modelling no guest cache and therefore
passing — is well supported, but one link is host-side and unverifiable from this repo:
KVM's EPT memory type for the `ram_device` memslot QEMU creates for a mapped blob. If
KVM sets `IPAT` with WB, the guest PTE is ignored and **no guest-side change can fix
it**. Discriminator: if `s0_submit` still hangs with the patch applied *and* the new
serial line confirms the feedback blob took the uncached path, the answer is host-side
and the next step is QEMU/KVM, not more kernel work. Note also that aarch64/HVF
**cannot** corroborate the "TCG masks it" hypothesis — that needs a host-visible
virtio-gpu blob under hardware virtualization, which needs EGL, which macOS lacks. The
aarch64 half is a latent-bug fix with no reachable failing test today; its evidence is
the MAIR read.

### 3. `ATTR_NOCACHE` on aarch64 is Device memory, not Normal-NC

`arch/aarch64/src/paging.rs:21` names MAIR index 3 "normal NC", but neither Limine nor
our direct-boot path programs attributes 2..7, so index 3 is zero = **Device-nGnRnE**.
Consequences beyond the blob work: **the framebuffer (`arch/aarch64/src/lib.rs:116`)
has silently been Device memory all along**, working only because `pitch = width*4`
keeps every access aligned — an unaligned framebuffer access would fault, and any future
code doing one would look like a mysterious alignment bug. `drivers/src/snd.rs:244`
maps an MMIO BAR with the same flag and *correctly wants* Device, which is exactly why
the blob fix introduces a separate `WRITECOMBINE` flag rather than redefining
`NOCACHE`. The comment must be corrected whether or not the blob patch lands, and the
framebuffer's attribute should be reconsidered deliberately rather than by accident.

### 4. x86_64 has no PAT or MTRR setup

Grepping `arch/x86_64/` finds `wrmsr` only for APIC, EFER, STAR, LSTAR and GSBASE — no
`IA32_PAT`, no MTRR. The reset PAT therefore applies, so `PageFlags::NOCACHE` (PCD)
selects UC and **true write-combining is unreachable**. Not currently a problem — UC is
strictly stronger and correct wherever we use it — but it is a real ceiling on
framebuffer and blob write throughput, and anything that eventually wants WC
performance needs `IA32_PAT` bring-up first.

### 5. Vulkan renders on LeandrOS; next is presenting it

**GPU work now executes.** `vkrender` was built, staged (`b2260b4`) and run: subtest 0
(shaderless `vkCmdFillBuffer`) passes with `fence signalled after 86 ms` and `all 65536
words == 0xdeadbeef` — **the first GPU work ever submitted from LeandrOS**. Subtest 1
(compute) passes with all 4096 words matching `(i*2654435761)^0x9E3779B9`. Subtest 2
(graphics) rasterizes a triangle: all 13 named pixel coordinates correct,
`s2_coverage: triangle=18432 clear=47104 other=0` — **exactly the analytic area** of a
192x192 right triangle — and `s2_no_intermediate_pixels` passes. Shaders compiled on
both arches; no SKIPs. `--- vkrender done, failures = 0, skipped = 0 ---`.

**`s2_checksum = 0x02C0FDC5` is byte-identical across x86_64/KVM, x86_64/TCG and
aarch64/TCG**, so it is now a pinned regression value — set
`VKRENDER_EXPECT_CHECKSUM=0x02C0FDC5`.

`run-qemu.sh --venus` landed in `b2260b4` and reproduces the bespoke wave scripts:
`venustest` 68/68 and `vktest` 0 failures on both arches, through the in-tree script.
OVMF still gets its GOP (post-login screendump 1920x1080 at 1.96% non-zero, against
1.97% on the default path), and the default path is unchanged on both arches.

**`driver.py`'s GL gate is answered, and the finding gates presentation too.** Measured
with the exact `--venus` device line: QMP `screendump` **does** work under `-display
egl-headless`, but only in its **bare** form (no arguments) — that captures the primary
console, q35's implicit std-VGA, giving a valid non-blank 1280x800 PPM. Passing
`device=<gl-dev-id>` fails with `"no surface"`, with or without `head=0`, because the GL
device has no surface. So a `driver.py` Venus mode is unblocked — it simply must not
pass `device=` — and the same constraint is why `--present` (below) blits to a real
scanout surface rather than relying on `screendump` of the GL device.

**Still true, kept from the earlier design pass.** The Linux-box environment
(`forain@172.16.158.150`, EndeavourOS, virglrenderer 1.3.0, QEMU 11.0.1 — already
installed, nothing to add; it is **Arch, not Debian**). macOS has no EGL, so
venustest's ~29 failures there are a host artifact, not a code defect. The loader stays
unshipped: the ICD exports only `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr` — no
`vkGetInstanceProcAddr` — so it can never stand in for `libvulkan.so.1`; `vkrender`
bootstraps the way `vktest` does and resolves device entry points via
`vkGetDeviceProcAddr`.

**Build findings, load-bearing for anyone rebuilding it.** `vkrender.c` needed **zero**
source changes, but the recipe needed three. `-std=c11` does **not** compile against
musl — strict ISO hides `clock_gettime`, `nanosleep` and `CLOCK_MONOTONIC`; use
`-std=gnu11` (the Mac's `-fsyntax-only` used laxer headers and missed this). Vulkan
headers need `/usr/include/vk_video` as well as `/usr/include/vulkan`
(`vulkan_core.h:9744` includes it), copied to a private dir — do not point `-I` at
`/usr/include`, it shadows the target libc's headers. And the container recipe
**cannot build aarch64 on that box**: no docker, and podman pulls arm64 images but
cannot execute them (no `binfmt_misc` aarch64 handler, only the dynamic
`qemu-aarch64`); cross-compiling with the artifacts repo's `zig cc` +
`musl-dyn-link.sh` works, with two gotchas — zig cc enables UBSan by default (link
fails on `__ubsan_handle_*`, needs `-fno-sanitize=undefined`) and its driver silently
produces a **static** binary, which cannot `dlopen` the ICD. Corrected recipes are at
`~/code/leandros-artifacts/notes/m9-m3-vulkan/build-vkrender-alpine-fixed.sh` and
`build-vkrender-aarch64-zig.sh`.

**Next is presentation.** `--present` (a dumb-buffer blit reusing `drmsmoke`'s
`ADDFB2`/`SETCRTC` sequence) is written and staged but unrun; it needs COSMIC stopped,
since we never gate `SETCRTC` on DRM master. After that, M4 is a Wayland client, still
blocked on the `PRIME_HANDLE_TO_FD` gap (item 6).

**Linux-box tree state (trap).** That checkout is on a **detached HEAD** with two
stashes, and **`stash@{0}` must not be blind-popped**. It holds 6 files but only 3 are
wanted (the AF_INET work, which has since landed as `26eebf0`); its `arch/*/src/timer.rs`
are now identical to what landed, and its `kernel/src/syscall.rs` is **older** than
current HEAD — popping it would revert `4085b7f` (nested-epoll readiness). Re-land from
`~/code/leandros-artifacts/notes/m9-af-inet-loopback/af_inet_loopback_verified.patch`
instead. A raw copy of that stash also exists at
`/home/forain/linux-tree-preexisting.patch` on the box.

### 6. PRIME export for blob handles — fix prepared (headless WSI unblocked)

**Why it rejects.** `kernel/src/syscall.rs:6052` calls `dumb_buffer_phys_order(handle)`,
whose entire body (`drivers/src/drm_device_interface.rs:1286-1288`) is
`DUMB_BUFFERS.lock().get(&handle)`. Blob BOs live in a **separate** map,
`BLOB_BUFFERS` (`:855`), with handles from `NEXT_BLOB_HANDLE` starting at `0x4000`
(`:858`) precisely so the two spaces cannot collide — so the lookup always misses and
`:6054` returns `-22`.

**Fixing only the lookup would have been worse than the EINVAL.** `install_dmabuf_vmo`
(`servers/vfs/src/lib.rs:560`) unconditionally built `1<<order` frames from `phys`, and a
`BLOB_MEM_HOST3D` blob has `phys == 0` (`drivers/src/drm_device_interface.rs:3487-3493`).
A successful lookup would have handed out **physical page 0 onward**.

**The export is plumbing; cross-open dmabuf is a subsystem, and the line is clean.**
Measured in Mesa 25.3.6: `wsi_create_native_image_mem` → `wsi_init_image_dmabuf_fd`
(`wsi_common_drm.c:726-739`) issues `GetMemoryFdKHR` for **every** swapchain image on
every `WSI_IMAGE_TYPE_DRM` path and propagates its error, and it is also a bare feature
probe on a 4 KiB device-local allocation (`:122-147`) — so the export must work for a
blob with neither guest pages nor a host-visible mapping. There is no escape hatch:
Venus is never `wsi_device->sw` (`wsi_common.c:87`), so the wl_shm branch is unreachable
for us. **Nobody mmaps the exported fd** — Venus maps BOs via `VIRTGPU_MAP` +
`mmap(gpu->fd, offset)`, and even kms_swrast's importer does `drmPrimeFDToHandle` +
`lseek(SEEK_END)` for the size, then `MODE_MAP_DUMB` on the *imported handle*.

What each consumer needs: `VK_EXT_headless_surface` — a valid fd, nothing more; Venus
self-import — `PRIME_FD_TO_HANDLE` + `RESOURCE_INFO` on the **same** open;
`VK_KHR_display` and Wayland dmabuf — import into a **different** DRM open, and for
Wayland a different process. That second tier needs cross-open BO reachability (our
`open_may_reach`, `drivers/src/drm_device_interface.rs:1091`, refuses **by design**),
host-resource refcounting across opens (`free_blob` today unconditionally unrefs and
releases the window span), `CTX_ATTACH_RESOURCE` for the importer's context, `MAP_DUMB`
and `ADDFB2` accepting blob handles, and for real scanout `SET_SCANOUT_BLOB`, which does
not exist here — plus the connector's missing `DPMS` property. Several days; deliberately
not speculated into a patch.

**The design.** `prime_export_backing(handle, open_id)` resolves blobs through the
owner-scoped `blob_lookup` (`b80ab5a`'s rule) and falls through to the
deliberately-global `dumb_buffer_phys_order`, reusing each registry's existing rule
rather than inventing a third. It returns `{phys, order, len}` — `len` is the *resource*
size for a blob, since Mesa's importer takes `lseek(SEEK_END)` verbatim, and the buddy
block for a dumb buffer, byte-identical to today because GBM/EGL fstat it. A HOST3D
blob's backing is `map_phys = window.phys + win_off`, a PCI BAR range **never in the
HHDM** and often not even reserved at export time, so the fd is a **token**: correct
`len`, correct `dmabuf_handle`, an **empty page list**, and mmap failing cleanly.

**That last part is what made it more than three lines,** and auditing it found three
pre-existing hazards on the dumb path. `vmo_acquire_frames`
(`servers/vfs/src/lib.rs:644-647`) grows *any* VMO on demand with
`vmo_alloc_zeroed_frame()`, so a page-less export would have silently satisfied an mmap
with zeroed anonymous memory — a coherence bug presenting as a Vulkan bug. On the dumb
path that growth is leaked outright (`vmo_free_slot:450` returns early for `borrowed`
without freeing); the write path grows the same way (`:3303-3305`); and
`handle_ftruncate` (`:5039`) would either leak on grow or, on shrink, `unref_or_free`
DRM-owned frames — order-0 frees out of an order-N buddy block, i.e. allocator
corruption. All three are closed by one stated rule: **a borrowed VMO's page list is
immutable.**

**Cacheability is avoided by construction, not luck.** The only mmap-able exports this
creates are guest RAM, which the queued `blob_map_cache_type` deliberately does not
match (`map_phys != 0`) and which is coherent write-back anyway. Host-visible blobs get
**no mmap-able export at all**, so no second code path can disagree with the host's
`map_info`. The constraint is written into the new doc comments.

**Patch prepared** at
`~/code/leandros-artifacts/notes/m9-prime-export/prime_handle_to_fd.patch` (4 files,
+308/−31, of which 150 lines are the regression subtest and most of the rest is
comment), `git apply --check`-clean and round-trip verified at `9d27ae0`, all four files
`rustfmt`-parse, **not built**. It stacks with all four other queued patches in **both**
orders with identical resulting trees, and also applies over the uncommitted in-flight
`drivers/` work. Worth recording: the first draft *deleted* `dumb_buffer_phys_order`,
whose doc comment `fb_damage_clips.patch` uses as trailing context, and conflicted in
both orders — keeping the function and calling it from `prime_export_backing` is better
design anyway.

**Verification.** `venustest` **68/0 → 77/0** (9 new reports). `drmsmoke` stays **22/0**
— `PRIME_HANDLE_TO_FD`, `PRIME_MMAP_ALIAS` and `PRIME_FD_TO_HANDLE` remaining PASS is
the dumb-path non-regression gate, and the one thing checkable **locally on the Mac**.
`scmtest` and `vkrender` (`s2_checksum = 0x02C0FDC5`) must not move. Everything HOST3D
needs the Linux box. Guard-test discipline is satisfied: HEAD is the backed-out state
for the two export subtests, so they must FAIL against an unpatched kernel; reverting
the `len` change must make the size subtest report `0x4000` instead of `0x3000`; and
`phase5_host3d_export_is_not_mappable` **must** be demonstrated to fail with its guard
line deleted, since it carries the whole safety argument. The decisive downstream test
is a `VK_EXT_headless_surface` swapchain — reachable with this patch alone, unlike
Wayland or display.

### 7. `SIMULATE_SYNCOBJ`: we reject the probe, and Mesa then closes stdin

`sim_syncobj_create` (`vn_renderer_virtgpu.c:145-190`) lazily submits an execbuffer with
`size=0, command=0` plus `FENCE_FD_OUT` and requires `args.fence_fd >= 0`; we reject at
`drivers/src/drm_device_interface.rs:3081` (`exec.command == 0 || exec.size == 0`) and
never write `fence_fd` back (`:3177-3190` logs it as ignored). **New and worse:**
`sim_submit` (`vn_renderer_virtgpu.c:531-557`) sets `FENCE_FD_OUT` whenever
`batch->sync_count != 0` and then calls `close(args.fence_fd)` — with `fence_fd` left at
its zero-initialised value that is **`close(0)`, closing stdin**. Whatever fix lands
must write `fence_fd` before that path is reachable. A signalled `eventfd2(1)` is the
right shape (~40 lines), correct because `submit_3d` is synchronous and Mesa only
`poll(POLLIN)`s the fd. Mesa 25.3.6 defines `SIMULATE_SYNCOBJ`/`SIMULATE_SUBMIT`
unconditionally, so this is not opt-in.

### 8. Borrowed VMOs can be grown, leaked and truncated

`vmo_acquire_frames` (`servers/vfs/src/lib.rs:644-647`) grows any VMO on demand with
`vmo_alloc_zeroed_frame()`, including borrowed ones backing DRM buffers; the growth is
then leaked, since `vmo_free_slot` (`:450`) returns early for `borrowed` without
freeing. The write path grows the same way (`:3303-3305`). Worst, `handle_ftruncate`
(`:5039`) on shrink would `unref_or_free` DRM-owned frames — order-0 frees out of an
order-N buddy block, i.e. **allocator corruption**. The rule that closes all three: a
borrowed VMO's page list is immutable. The queued PRIME patch (item 6) states and
enforces it; if that patch does not land, these remain open independently.

### 9. Cross-open dmabuf import is refused by design

`open_may_reach` (`drivers/src/drm_device_interface.rs:1091`) deliberately scopes BOs to
their owning DRM open, which is correct for `b80ab5a`'s ownership model but blocks
`VK_KHR_display` and Wayland dmabuf, both of which import into a different open (and for
Wayland, a different process). Supporting them needs cross-open reachability with
host-resource refcounting across opens, `CTX_ATTACH_RESOURCE`, `MAP_DUMB`/`ADDFB2`
accepting blob handles, `SET_SCANOUT_BLOB` (absent), and the connector's missing `DPMS`.
Several days. This is the M4 gate; headless WSI does not need it.

### 10. Primary-plane recomposite (FB_DAMAGE_CLIPS is the instrument, not the fix)

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

### 11. `listen()` twice returns EINVAL — fix prepared

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
**`scmtest` 28/0 → 29/0** — the memfd/TGID patch's two subtests already landed in
`77f170d` and are part of the 28/0 baseline.

### 12. `unused variable: port` in `handle_close` — not a leak, fix prepared

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

### 13. Delete the unreachable `init-server` crate

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

### 14. AF_UNIX `listen()` is lax in the opposite direction

The AF_UNIX arm of `handle_listen` is an unconditional `ok_reply()` — a repeat listen
already succeeds, but so does `listen()` on an unbound or already-connected AF_UNIX
socket, where Linux answers EINVAL. Found while fixing the AF_INET side (item 11) and
deliberately **not** changed there: tightening it alters behaviour for every AF_UNIX
server on the system (cosmic-comp, busd, tokio) and could not be validated in a
read-only session. Needs a live COSMIC session to land safely.

### 15. No TIME_WAIT — ports are instantly reusable

`handle_close` calls `socket_set.remove()` immediately, so a closed TCP port can be
rebound at once where Linux would hold it in TIME_WAIT. A divergence, not a leak, and
low priority — but it is the kind of thing that makes a server restart behave
differently here than on Linux.

### 16. Deferred work and known limitations

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
- **Build gotcha, cost two QEMU cycles during the `77f170d` verification:** building a
  userland test binary with a bare `cargo build` instead of `scripts/build-userland.sh`
  omits `-C relocation-model=static`, producing a PIE whose `.data.rel.ro` our loader
  never relocates. It then faults at `__libc_start_main+0x44` with `CR2=0`, before
  `main` — a distinctive signature whose cause is not obvious from the fault alone.
  Always build userland through `scripts/build-userland.sh`.
- **`driver.py` still has no Venus/GL mode.** Unblocked (item 5): QMP `screendump`
  works under `-display egl-headless` in its bare form, without `device=`. The mode
  itself — teaching `.claude/skills/run-leandros/driver.py:_build_cmd` to build the
  `--venus` device line and call `screendump` bare — still needs writing.

---

## Housekeeping

- Untracked disk-image backups at the repo root
  (`f2fs-data0-aarch64.img.12h15-orig`, `.full-rebuild`, `.m7z2-orig-backup`,
  `f2fs-data0-x86_64.img.m7z2bak`) and `ports/busd/.work/` are now gitignored
  (`f2fs-data0-*.img.*`, `ports/*/.work/`); delete them by hand when no longer needed.
- Run regression harnesses with `python3 -u` and **no pipe**: buffering makes a healthy
  background run look like a crash, and piping through `tail` gets the run reaped at
  exit 144.
