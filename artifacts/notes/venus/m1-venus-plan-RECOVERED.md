# Vulkan-via-Venus on LeandrOS (virtio-gpu → QEMU/virglrenderer)

## Context

Goal: run `vkcube` on LeandrOS x86_64 under QEMU, using the Venus virtio-gpu
context type (a wire-protocol passthrough of Vulkan calls to the host's
virglrenderer/venus render server — QEMU's `virtio-gpu-gl-pci` device with GL
already runs in `run-qemu.sh`). This is not "write a rasterizer" — it's three
layers of plumbing: (1) kernel virtio-gpu driver support for 3D contexts, blob
resources, and real command submission, (2) a `/dev/dri/card0` ioctl surface
that matches Linux's `virtgpu_drm.h` uAPI closely enough for Mesa's existing
venus driver code, (3) getting a vendored subset of Mesa's venus Vulkan ICD
(plus vkcube) to build and run statically against relibc.

Per user direction: **x86_64 first** (aarch64 port deferred to a later phase,
verified only at major milestones rather than every commit), **offscreen
render-to-screenshot as the first working milestone** (on-screen `VK_KHR_display`
presentation is a follow-up phase), and all of this happens in an **isolated
git worktree** so main stays bootable throughout.

This plan covers the full architecture so the shape of the multi-phase effort
is visible, but execution is milestone-gated: each milestone must actually
boot and verify in QEMU before the next one starts. Milestone M1 (kernel +
DRM Venus plumbing, proven by a smoke-test program, no Mesa yet) is the
concrete unit of work this plan commits to executing now. M2+ (vendoring
Mesa, vkcube) is scoped architecturally but will be re-planned in detail once
M1's real host-protocol behavior is observed — a lot of the Venus ring/async
semantics can only be pinned down by testing against the actual host
virglrenderer.

## Current state (from research)

- `drivers/src/virtio_gpu.rs`: PCI transport works, feature negotiation is
  hardcoded to 0 (`driver_feature = 0`, line ~386) — no `VIRTIO_F_VERSION_1`,
  `VIRTIO_GPU_F_VIRGL`, `F_RESOURCE_BLOB`, or `F_CONTEXT_INIT` bits are ever
  negotiated. `VirtioGpuCmd` enum command IDs diverge from upstream
  `virtio_gpu_hw.h` from `Submit3d` onward (3D commands live in the `0x0200`
  range upstream; this driver has them contiguous with 2D at `0x01xx`) — this
  must be corrected against QEMU's own header at implementation time, wire
  compatibility depends on exact numeric match with the host. No `CTX_CREATE`/
  `CTX_ATTACH_RESOURCE`/blob commands exist at all. Submission is synchronous
  busy-spin polling only (no IRQ handler wired despite the ISR capability
  being mapped), one page (4 KiB) hard cap on command payload — no
  scatter-gather for larger command streams, no fence_id tracking (`fence_id`
  always hardcoded 0, so `VIRTIO_GPU_FLAG_FENCE` is unused).
- `drivers/src/drm_device_interface.rs`: `VIRTGPU_EXECBUFFER`/`RESOURCE_CREATE`/
  `TRANSFER_*`/`GET_CAPS` ioctls exist but are stubs — `virtgpu_handle_execbuffer`
  never reads `_exec.command`/`_exec.size` (the actual command bytes are
  discarded, `Submit3d` is sent with `&[]`); `GET_CAPS` never copies capset
  bytes back to userspace. `VIRTGPU_CONTEXT_INIT`, `VIRTGPU_RESOURCE_CREATE_BLOB`,
  `VIRTGPU_GETPARAM`, `VIRTGPU_MAP`, `VIRTGPU_WAIT` don't exist yet (only
  commented-out placeholder constants). No `copy_from_user` abstraction exists
  or is needed — `servers/drm` runs ioctl handlers synchronously in the
  caller's address space, so `arg: usize` is a directly-dereferenceable
  userspace pointer already (see `servers/drm/src/lib.rs:73`).
- `ipc::Message`'s `cap`/`has_cap` fields (the documented "large payload via
  shared-memory capability" mechanism) are **dead code** — never read or
  written anywhere. The actual working large-buffer pattern, used by every
  framebuffer/dumb-buffer path today, is: allocate physically-contiguous pages
  via `mm::buddy::alloc`, hand back the physical address as an ioctl
  out-param/mmap offset, let userspace `mmap()` it via the existing device-fd
  mmap path in `sys_mmap` (`kernel/src/syscall.rs`, real `MAP_SHARED` for
  device fds, confirmed working — regular file-backed `MAP_SHARED` is NOT
  implemented, but irrelevant here since GPU buffers always go through a
  device fd). Reuse this pattern for blob resources and for command-stream
  buffers larger than one page — no IPC changes needed.
- Toolchain for C/C++ userland: `zig cc`/`zig c++` targeting `<arch>-linux-musl`
  with `--sysroot=<relibc sysroot>`, static linking only (see
  `mame/Makefile.leandros`). Zig bundles its own libc++, so C++ builds too
  (used by the MAME port). This is the toolchain to use for the Mesa/vkcube
  port.
- **Dynamic linking (`dlopen`/PT_INTERP/ld.so) does not work on LeandrOS** —
  relibc's ld.so code is real but never invoked, because the kernel ELF
  loader only supports `ET_EXEC` and explicitly ignores `PT_INTERP`
  (`elf/src/lib.rs`). This rules out a real Khronos Vulkan Loader (which
  dlopen's ICDs at runtime) for now. **Consequence: statically link the
  vendored venus ICD directly into vkcube**, calling its `vkCreateInstance`
  etc. entrypoints directly — skip the loader layer entirely. This is a
  standard simplification for bring-up and matches how e.g. some embedded/test
  harnesses use Mesa's ICD directly.
- pthreads (futex-backed, real `SYS_CLONE`), rwlock, mutex, cond, TLS,
  `clock_gettime` are all solid and already exercised by `pthreadtest` — fine
  foundation for Mesa/Vulkan's internal worker threads.

## Architecture / Phases

**M1 — Kernel + DRM Venus transport, proven by a smoke test (this session's deliverable)**
1. `drivers/src/virtio_gpu.rs`:
   - Fix feature negotiation: read device features properly (feature select 0
     and 1 for the high 32 bits), ack `VIRTIO_F_VERSION_1`, `VIRTIO_GPU_F_VIRGL`,
     `VIRTIO_GPU_F_RESOURCE_BLOB`, `VIRTIO_GPU_F_CONTEXT_INIT` (fail loudly if
     host doesn't offer them rather than silently degrading).
   - Correct all `VirtioGpuCmd` numeric values against QEMU's
     `include/standard-headers/linux/virtio_gpu.h` (2D commands stay in
     `0x01xx`, 3D/context commands go in `0x02xx` — must match exactly for
     virglrenderer to parse the ring correctly).
   - Add `CTX_CREATE` (with `context_init` = Venus capset id, name string),
     `CTX_DESTROY`, `CTX_ATTACH_RESOURCE`, `CTX_DETACH_RESOURCE`,
     `RESOURCE_CREATE_BLOB`, `RESOURCE_MAP_BLOB`, `RESOURCE_UNMAP_BLOB`,
     `GET_CAPSET_INFO` (separate from `GET_CAPSET`, currently conflated).
   - Replace the 4 KiB single-page command payload cap with a scatter-gather
     submission path (multi-descriptor or a separately-mapped multi-page
     buffer referenced by the command) — Venus command streams routinely
     exceed 4 KiB.
   - Add real fence handling: set `VIRTIO_GPU_FLAG_FENCE` + monotonically
     increasing `fence_id` on submits that need it, and register the ISR
     interrupt handler (currently captured but unused) instead of pure
     busy-spin, so multiple commands can be in flight.
2. `drivers/src/drm_device_interface.rs`:
   - Add `VIRTGPU_CONTEXT_INIT`, `VIRTGPU_RESOURCE_CREATE_BLOB`,
     `VIRTGPU_GETPARAM`, `VIRTGPU_MAP`, `VIRTGPU_WAIT` ioctls with struct
     layouts field-for-field matching Linux's `virtgpu_drm.h` (Mesa's venus
     renderer backend issues these exact structs via raw `ioctl()` — no
     libdrm dependency needed, Mesa vendors its own copy of the header).
   - Fix `virtgpu_handle_execbuffer` to actually read `_exec.command`/`.size`
     and forward the real byte stream (via the mmap/physical-buffer pattern
     above for anything over one page) instead of discarding it.
   - Fix `GET_CAPS` to copy the real capset blob bytes back to `_caps.addr`/
     `.size` instead of ignoring them.
   - Wire `GETPARAM` to answer `VIRTGPU_PARAM_3D_FEATURES`,
     `VIRTGPU_PARAM_CAPSET_QUERY_FIX`, `VIRTGPU_PARAM_RESOURCE_BLOB`,
     `VIRTGPU_PARAM_CONTEXT_INIT`, `VIRTGPU_PARAM_HOST_VISIBLE` truthfully —
     Mesa's probing code refuses to proceed if these read back wrong.
3. New userland smoke test `userland/venustest` (same pattern as
   `pthreadtest`/`polltest`/`forktest`): open `/dev/dri/card0`, `GETPARAM`
   probe, `CONTEXT_INIT` with Venus capset id, `GET_CAPSET_INFO`+`GET_CAPSET`
   and assert a non-trivial (non-zero-length, host-populated) capset blob
   comes back — this alone proves the host's virglrenderer recognized a
   Venus context and is alive/talking — then `RESOURCE_CREATE_BLOB` +
   `VIRTGPU_MAP` + write/readback, and one `EXECBUFFER` submit with a fence
   wait via `VIRTGPU_WAIT`. No actual Vulkan/Venus wire protocol encoding
   yet (that's Mesa's job in M2) — this only proves the transport, context,
   blob, and fence layers work end-to-end against the real host.

**M2 — Vendor Mesa's venus Vulkan ICD (future phase, re-planned after M1 lands)**
- Pin a Mesa version, run Mesa's own Python/Meson codegen (vk.xml →
  dispatch tables/entrypoints via `src/vulkan/util`'s generators, venus's own
  `vn_protocol_driver_gen.py`) on a host dev machine, and vendor the
  generated + hand-written sources (`src/virtio/vulkan/vn_*.c/h`,
  `src/vulkan/util`, `src/util`) into the tree under something like
  `third_party/mesa-venus/`.
- Port `vn_renderer_virtgpu.c` (Mesa's own virtgpu ioctl backend — it talks
  directly to `DRM_IOCTL_VIRTGPU_*` via raw `ioctl()`, no libdrm build
  required) onto relibc's ioctl/mmap syscalls.
- Build with `zig cc`/`zig c++` against the relibc sysroot, statically.
- First real milestone here: enumerate a `VkPhysicalDevice` — the first
  actual round-tripped Vulkan call through Venus.

**M3 — vkcube, offscreen (deliverable milestone user selected)**
- Statically link vkcube directly against the vendored ICD's entrypoints
  (no Khronos loader, per the dlopen limitation above).
- Render a frame into a host-visible blob resource, transfer out, dump via
  the existing screenshot workflow (`run-leandros` skill) as PNG/PPM.

**M4 — On-screen presentation (future phase, not in this plan's scope)**
- `VK_KHR_display` direct-to-KMS-plane presentation using the existing
  `drivers/src/drm/*` KMS subsystem, since there's no compositor/Wayland/X.

**M5 — aarch64 port (future phase)**
- Re-verify/port the M1–M3 work for aarch64 once x86_64 vkcube works.

## Verification (M1)

- `./scripts/build-all.sh` (release only, x86_64) builds cleanly.
- `./scripts/run-qemu.sh x86_64` boots to userland.
- Run `venustest` from the LeandrOS shell; expect: capset blob length > 0,
  blob resource create+map+readback matches what was written, `EXECBUFFER`+
  `VIRTGPU_WAIT` completes without the old busy-spin timeout path firing.
- Check host-side QEMU/virglrenderer log output (if `run-qemu.sh` surfaces
  it, or via `-d` flags) for a recognized Venus context creation — this is
  the strongest signal the wire protocol is actually correct, not just
  "didn't crash."
- Re-run `venustest` a few times per boot (per existing project convention
  for catching leaks across repeated runs/exits, see prior poll/epoll work).

## Next step

Enter the isolated worktree, then start M1: virtio-gpu feature negotiation +
command-ID fixes first (smallest, most foundational change, needed before
anything else compiles/links correctly), then context/blob/fence commands,
then the DRM ioctl surface, then `venustest`.
