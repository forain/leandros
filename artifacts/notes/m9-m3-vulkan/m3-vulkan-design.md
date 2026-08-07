# M3 — Vulkan rendering milestone: design

Source analysis only. Nothing was built, run, or modified. Repo `/Users/forain/code/leandros`
at `aa2329c` (branch `aarch64-kernel-softfloat`); Mesa 25.3.6 source read at
`/Users/forain/code/leandros-artifacts/llvmpipe-lane/src/mesa`.

Every claim below is tagged **[M]** = measured (read in source / binary / archived log) or
**[I]** = inferred (reasoned from the measured facts; needs a run to confirm).

---

## 0. Executive recommendation

**Do not chase `vkcube`, and do not choose a WSI yet.**

Build **`vkrender`** — a dependency-free C binary in the exact shape of `vktest` (dlopen the
ICD, escalating PASS/FAIL subtests) that:

1. runs three escalating GPU submissions — a shaderless `vkCmdFillBuffer`, a compute
   dispatch, then a real graphics pipeline drawing a triangle into an offscreen image;
2. copies the result into `HOST_VISIBLE|HOST_COHERENT` memory, `vkMapMemory`s it, and
   **asserts specific pixel values** (corner == clear colour, centre == triangle colour) plus
   a whole-image checksum;
3. then blits those exact bytes into a DRM dumb buffer and `SETCRTC`s it — the
   `CREATE_DUMB → MAP_DUMB → mmap → ADDFB2 → SETCRTC` sequence `userland/drmsmoke` already
   proves works **[M]** (`/Users/forain/code/leandros/userland/drmsmoke/src/main.rs:362-425`).

That is *rendered Vulkan output visible on the LeandrOS screen* with **zero WSI, zero
Khronos loader, and (probably) zero new kernel code**.

This is not a preference; §3.0 establishes it as the only reachable option. **Every WSI path
on Venus — including `VK_EXT_headless_surface`, which sounds free — fails today at the same
single kernel gap:** `vkGetMemoryFdKHR` exports swapchain memory via
`DRM_IOCTL_PRIME_HANDLE_TO_FD`, and our intercept resolves handles only through
`dumb_buffer_phys_order`, so a Venus blob handle returns `EINVAL` **[M]**. Two kernel items
(§3.5) gate all WSI; both are small, and both are M4 work, not M3.

---

## 1. What is the smallest meaningful M3?

### The four candidates, ranked by proof-value ÷ work

| # | Milestone | Work | What it actually proves | Verdict |
|---|---|---|---|---|
| **A** | Offscreen render + CPU readback + pixel assertions (`vkrender`) | **S–M** (one ~600-line C file, precompiled SPIR-V, 4 lines in the mkfs script) | Command-buffer recording, `vkQueueSubmit`, fence/idle wait, the Venus ring under a *real* payload, host-visible memory mapped into the guest, **and that the host GPU rasterized the correct pixels** | **Do this first** |
| **A+** | A, then blit the readback to a DRM dumb buffer + `SETCRTC` | **+S** (~80 lines lifted from `drmsmoke`) | Everything in A, *plus* the milestone sentence "Vulkan output visible on screen in LeandrOS" | **Do this, same wave** |
| **C** | Vulkan client composited by cosmic-comp via `VK_KHR_wayland_surface`, **`wl_shm` path under `MESA_VK_WSI_DEBUG=sw`** | **L** | A real end-to-end desktop-integrated Vulkan app; needs no dma-buf and no KMS | **M4** — after the two kernel items in §3.5 |
| **B** | `VK_KHR_display` presenting directly on our DRM | **XL** | Direct-to-KMS scanout of a GPU-rendered image | No — §3.3 lists four hard blockers, one of them a whole dma-buf import/scanout subsystem |
| **D** | `VK_EXT_headless_surface` + swapchain | S… but **broken today** | Adds swapchain *API surface* but presents nowhere, and its images are `DEVICE_LOCAL`/unmappable so the app must still copy out itself | Skip — measurably more work than A for measurably less proof (§3.2) |

### Why the readback test is worth far more than it sounds

`vktest` today stops at `vkCreateDevice` **[M]**
(`/Users/forain/code/leandros-artifacts/venus-lane/vktest.c:1-21` documents its six steps;
the archived pass in `.../notes/m2c-venus-linux-2026-08-06/venuswave_x86_64_serial.log` ends
at `vkDestroyDevice`). That means **not one byte of GPU work has ever been submitted from
LeandrOS.** Everything proven so far is *device discovery*: capset negotiation, context init,
ring shmem, blob mapping.

A readback test crosses every remaining boundary in one step:

- **The ring under load.** Today the ring only carries small enumeration commands. A real
  command buffer means large `vn_cs` payloads, `bo_handles` arrays, and the `vn_ring` relax
  loop under sustained pressure — the exact place where the 10 ms `CLOCK_MONOTONIC`
  granularity bug (`75b32e3`) and the `nanosleep` truncation bug (`fb398c7`) both bit **[M]**
  (TODO.md item 1; MEMORY index).
- **Fences.** First real `vkWaitForFences`/`vkQueueWaitIdle`. See §5 for why this is the
  riskiest single thing in the plan.
- **App-allocated host-visible memory.** Today only *Mesa's own* ring shmem is blob-mapped
  (`[DRM] host-visible blob mapped: res=0x28 … map_info=0x1` **[M]**). An app
  `vkAllocateMemory` from a `HOST_VISIBLE` type takes the same `RESOURCE_CREATE_BLOB` +
  `VIRTGPU_MAP` path but at app scale and app lifetime.
- **Correctness, not liveness.** A checksum over rasterized pixels is the only test that can
  distinguish "the stack didn't crash" from "the GPU drew the right thing". Every LeandrOS
  bug in this lane so far (FP/SIMD clobber, `GETPARAM.value` pointer clobber, nanosleep) was
  a *silent wrong-value* bug that liveness tests passed.

The marginal cost of A+ over A is one memcpy and five ioctls we already ship.

### Concrete shape of `vkrender`

Escalating subtests, each PASS/FAIL logged independently — the `vktest` house style **[M]**:

```
 1. dlopen /usr/lib/libvulkan_virtio.so          (proven)
 2. vk_icdNegotiateLoaderICDInterfaceVersion     (proven)
 3. vkCreateInstance / EnumeratePhysicalDevices  (proven)
 4. vkCreateDevice + vkGetDeviceQueue            (proven)
 4b. log vkEnumerateDeviceExtensionProperties and every memory type's property
    flags + heap index. Ten lines; pre-answers §6 questions 3 and 6 for free.
 5. pick a HOST_VISIBLE|HOST_COHERENT memory type; vkCreateBuffer + vkAllocateMemory
    + vkMapMemory                                <-- first new ground
 6. SUBMIT-0 (shaderless): vkCmdFillBuffer(0xDEADBEEF) on a DEVICE_LOCAL buffer,
    vkCmdCopyBuffer into the host-visible one, submit, vkQueueWaitIdle, verify the
    pattern                                      <-- proves the whole submit/complete
                                                     loop with zero shader ambiguity
 7. SUBMIT-1 (compute): a 4-line SPIR-V compute shader writing gl_GlobalInvocationID
    into a storage buffer; readback + verify
 8. SUBMIT-2 (graphics): 256x256 R8G8B8A8_UNORM colour attachment, clear to blue,
    one triangle in red via a trivial vert/frag pair, vkCmdCopyImageToBuffer into the
    host-visible buffer, readback
 9. ASSERT pixels: (0,0) == clear colour, (128,160) == triangle colour, plus a FNV-1a
    checksum of the whole image printed to serial
10. PRESENT (A+): open /dev/dri/card0, GETRESOURCES/GETCONNECTOR, CREATE_DUMB at the
    connector's mode, MAP_DUMB + mmap, scale/centre the 256x256 image into it,
    ADDFB2, SETCRTC. Screen now shows a GPU-rendered triangle.
```

Build it the way `vktest` is built **[M]**
(`/Users/forain/code/leandros-artifacts/venus-lane/build-vktest-alpine.sh`): Alpine 3.21
container, native `cc`, `-fno-stack-protector -U_FORTIFY_SOURCE`, link the local
`ssp_guard.o` (LeandrOS musl `libc.so` has no `__stack_chk_guard`), `-ldl`, then
`patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so`. Add `glslang` to the `apk add`
line to compile the shaders, and emit them as `static const uint32_t[]` arrays so the guest
binary has no runtime shader dependency.

One implementation note the `vktest` precedent does not cover: `vktest` only ever needs
*instance*-level entry points, which it resolves with `icd_gipa(instance, "…")` **[M]**
(`vktest.c:130-160`). `vkrender` needs dozens of *device*-level ones, so it must resolve
`vkGetDeviceProcAddr` via `icd_gipa(instance, "vkGetDeviceProcAddr")` once and route every
per-device call through it. A small `X(name)` macro table keeps this to ~40 lines instead of
~40 repetitive blocks.

Staging is 2 lines next to the existing `vktest` entry **[M]**
(`/Users/forain/code/leandros/scripts/mkfs-f2fs-populated.py:610-612`) — no new DT_NEEDED
closure, because `vkrender` links exactly what `vktest` links.

---

## 2. The loader question

### Measured facts

`libvulkan_virtio.so` exports **exactly three** Vulkan-visible dynamic symbols **[M]**:

```
vk_icdNegotiateLoaderICDInterfaceVersion
vk_icdGetInstanceProcAddr
vk_icdGetPhysicalDeviceProcAddr
```

(plus `__stack_chk_guard` from the injected guard object). There is **no `vkGetInstanceProcAddr`,
no `vkCreateInstance`** — the ICD is not usable as a drop-in `libvulkan.so.1`.

DT_SONAME is `libvulkan_virtio.so`; DT_NEEDED is
`libz.so.1, libdrm.so.2, libwayland-client.so.0, libdisplay-info.so.3, libexpat.so.1, libc.so`
**[M]** — all already staged **[M]** (`mkfs-f2fs-populated.py:600-605`). The ICD manifest
`/usr/share/vulkan/icd.d/virtio_icd.<arch>.json` is already on the image too **[M]**
(`mkfs-f2fs-populated.py:617-624`), pointing at `/usr/lib/libvulkan_virtio.so`, api_version
1.4.328.

### What breaks for a real app

`vkcube` (and every upstream Vulkan app) links `-lvulkan` → `DT_NEEDED libvulkan.so.1` and
calls `vkCreateInstance` by symbol. On LeandrOS that binary would fail at `ld-musl` relocation
time: no such library, no such symbol. **[I], but a direct consequence of the export list.**

### Verdict: no loader for M3. Defer it, and when we do it, ship the real one.

**For M3: keep the `vktest` bypass.** `vkrender` bootstraps from
`vk_icdGetInstanceProcAddr(NULL, "vkCreateInstance")` exactly as `vktest` does — this is
precisely what a loader does internally, and it is already proven to work end-to-end on both
arches. Writing a real render loop against `vk_icdGetInstanceProcAddr` costs nothing extra:
the only difference from ordinary Vulkan is that you resolve function pointers yourself
instead of linking them.

**When a loader becomes necessary** (i.e. when we want to run *unmodified* upstream binaries —
vkcube, vkmark, vulkaninfo), build the **Khronos loader**, not a hand-written shim:

- *Cost*: it is a normal CMake/musl build, and Alpine even packages it (`vulkan-loader`), so
  the existing Alpine-container recipe extends to it. Small. **[I]**
- *What it needs at runtime*: `opendir`/`getdents` over `/usr/share/vulkan/icd.d` (and
  `/etc/vulkan/icd.d`, XDG dirs — absent dirs are non-fatal), reading + parsing the JSON
  manifest, `dlopen` of the `library_path`, `getenv`/`secure_getenv`, and pthreads. Our VFS
  serves directory enumeration on f2fs and `/usr/share/vulkan/icd.d` is *already* populated
  **[M]** — so the loader's discovery path is, unusually, the part most likely to just work.
  It also scans implicit-layer directories; missing ones are non-fatal. **[I]**
- *Why not a 200-line forwarding shim*: at ICD interface version ≥ 4 the loader/ICD contract
  requires the loader to own the dispatchable-handle "loader magic" on `VkInstance`,
  `VkPhysicalDevice`, `VkDevice`, `VkQueue`, `VkCommandBuffer`. A naive forwarder gets this
  subtly wrong and fails in ways that look like GPU bugs. `vktest` sidesteps it only because
  it hands the ICD's own handles straight back to the ICD. **[I], from the ICD-interface
  contract; the ICD negotiated version 5 in the archived log **[M]**.**

So: **loader = deferred M4/M5 work item, explicitly not on the M3 critical path.**

## 3. WSI choice

The build really does compile in wayland + display(KMS) + headless WSI and **not** X11 **[M]** —
confirmed by symbol presence in the shipped `libvulkan_virtio.so` (`wsi_display_*`,
`wsi_headless_*`, `wsi_wl_*`; no `wsi_x11_*`) and by the meson line `-Dplatforms=wayland` with
`VK_USE_PLATFORM_DISPLAY_KHR` enabled independently via `system_has_kms_drm`
(`src/vulkan/meson.build:57-60`). The ICD advertised 19 instance extensions in the archived
passing run **[M]**.

### 3.0 The finding that decides this section

> **Every WSI path on Venus dies at the same single kernel gap today:
> `DRM_IOCTL_PRIME_HANDLE_TO_FD` on a Venus blob returns `EINVAL`.**

Chain of evidence, all **[M]**:

- Venus exports swapchain image memory as a dma-buf: `vn_GetMemoryFdKHR`
  (`vn_device_memory.c:544-560`) → `vn_renderer_bo_export_dma_buf` →
  `virtgpu_ioctl_prime_handle_to_fd` (`vn_renderer_virtgpu.c:680-691`), which is a plain
  `DRM_IOCTL_PRIME_HANDLE_TO_FD` on the blob's `gem_handle`.
- Our `PRIME_HANDLE_TO_FD` is intercepted in `kernel/src/syscall.rs:6049-6108` and resolves the
  handle through `drivers::drm_device_interface::dumb_buffer_phys_order(handle)` — **dumb
  buffers only**. A blob handle is not in `DUMB_BUFFERS`, so it returns `-EINVAL` (`:6053-6055`).
- The common WSI reaches `vkGetMemoryFdKHR` on **all three** paths that matter:
  `wsi_create_native_image_mem` (`wsi_common_drm.c:725-740`) for `VK_KHR_display` **and** for
  `VK_EXT_headless_surface` (headless builds `wsi_drm_image_params` unconditionally,
  `wsi_common_headless.c:427-433`), and the dmabuf Wayland path.

So `VK_EXT_headless_surface` — the option that sounds like it needs nothing — is **also broken
today**, for the same reason. This retires option D on evidence rather than on taste, and it is
the strongest argument for the §1 recommendation: the offscreen readback test is not merely the
smallest meaningful M3, it is **the only Vulkan-output milestone reachable without new kernel
work**.

### 3.1 The second cross-cutting blocker: Venus's fence-fd path is dead on our kernel

Mesa 25.3.6 `src/virtio/vulkan/vn_renderer_virtgpu.c:40-42` **[M]**:

```c
/* XXX comment these out to really use kernel uapi */
#define SIMULATE_BO_SIZE_FIX 1
#define SIMULATE_SYNCOBJ     1
#define SIMULATE_SUBMIT      1
```

Unconditional. Consequences, all **[M]**:

- No `DRM_IOCTL_SYNCOBJ_*` is ever issued — syncobjs are simulated in userspace, and waits are
  `poll(fd, POLLIN)` on a fence fd (`sim_syncobj_poll`, `:218-236`).
- Submission goes through `sim_submit` (`:518-565`), which sets `VIRTGPU_EXECBUF_FENCE_FD_OUT`
  **iff `batch->sync_count != 0`**.
- `sim_syncobj_create` (`:144-192`) lazily mints a known-signalled fd by issuing an execbuffer
  with **`.size = 0, .command = 0`** + `FENCE_FD_OUT`, and gives up entirely (returns handle 0)
  if that ioctl fails.
- We reject that ioctl at **two** layers: `drm_device_interface.rs:2941`
  (`if exec.command == 0 || exec.size == 0 { return Err(InvalidParameter) }`) and
  `virtio_gpu.rs:1856` (`if cmds.is_empty() { return Err(()) }`). And even on a valid submit we
  never write `fence_fd` back — the driver logs `EXECBUFFER: fields asked for but NOT honoured —
  fence_fd` and drops it (`:3037-3070`).

Where `vn_renderer_sync` is actually reached **[M]**:

| Caller | When | Impact |
|---|---|---|
| `vn_renderer_util.c:9` ← `vn_ring.c:388` (ring destroy) | instance teardown | benign — `vktest` already logs `vkDestroyInstance done` **[M]** |
| `vn_queue.c:1819 vn_create_sync_file` ← `vkGetFenceFdKHR` / `vkGetSemaphoreFdKHR` | **WSI acquire/present sync** | fatal for WSI |

Ordinary `vkQueueSubmit` does **not** touch it — it rides the ring with `ring_seqno` +
`vn_feedback` (`vn_queue.c:1026-1049`) **[M]**. *That is precisely why the offscreen readback
test can succeed today while every WSI path cannot.*

There is a related gate worth recording: Venus advertises `VK_KHR_swapchain` **only if**
`physical_dev->renderer_sync_fd.semaphore_importable` (`vn_physical_device.c:1175-1185`) **[M]**.
That flag reflects the *host* renderer's capabilities (RADV supports SYNC_FD import), so the
extension is probably advertised — but its use funnels straight into the dead path above. **[I]**

**The fix is small and belongs at the syscall layer.** Our `submit_3d` is **synchronous** — the
host has completed the work by the time `EXECBUFFER` returns **[M]**
(`drm_device_interface.rs:2913-2924` "while submission is synchronous"; `virtio_gpu.rs:1885`
`submit_checked` blocks for the response). An *always-already-signalled* fence fd is therefore
semantically correct, and Mesa only ever `poll(POLLIN)`s it.

> Accept `size == 0 && command == 0` as a **fence-only submit that issues no device command at
> all** (short-circuit *before* `submit_3d`; a zero-length stream must never reach the
> virtqueue), and on `FENCE_FD_OUT` return an `eventfd2(1, EFD_CLOEXEC)` — permanently
> `POLLIN`-ready. Implement it in `kernel/src/syscall.rs` alongside the existing PRIME
> intercept (`:6041-6120`), which is already the established place where a DRM ioctl must mint
> an fd because "the driver layer has no channel to create one" — the exact reason the in-code
> comment gives for `fence_fd` being unimplemented **[M]**. `sys_eventfd2` exists
> (`syscall.rs:7012`) **[M]**. ~40 lines. **[I] on sufficiency; [M] on every fact beneath it.**

### 3.2 `VK_EXT_headless_surface`

*Needs from us:* the dma-buf export of §3.0 — so it is **broken today**, not free **[M]**.
*Value if fixed:* `vkQueuePresentKHR` is literally two lines that mark the image not-busy
(`wsi_common_headless.c:355-359`) **[M]**, and the images are `DEVICE_LOCAL`, not mappable
(`wsi_select_device_memory_type`, `wsi_common.c:1952`) **[M]** — so the app must still do its
own `vkCmdCopyImageToBuffer` to see anything. That is *exactly* what §1 does, minus a swapchain.
**Skip: strictly more work than §1 for strictly less proof.**

### 3.3 `VK_KHR_display` / `VK_EXT_acquire_drm_display` — the DRM gaps, precisely

We own the DRM side, so this looks attractive. It is not, but the gap list is now exact.

**Good news first — three things that would have been plausible blockers and are not:**

- **The atomic property model is complete enough.** `wsi_common_display.c` fails hard unless
  `find_properties` (`:281-383`) locates every non-optional property **[M]**:
  connector needs `CRTC_ID` + `DPMS`; CRTC needs `MODE_ID` + `ACTIVE`; the primary plane needs
  all ten of `FB_ID, CRTC_ID, CRTC_X, CRTC_Y, CRTC_W, CRTC_H, SRC_X, SRC_Y, SRC_W, SRC_H`.
  Against our tables (`drm_device_interface.rs:151-166`, `object_props` `:204-240`) **[M]**:

  | Object | Mesa requires | We expose | Verdict |
  |---|---|---|---|
  | Plane 30 | the 10 above | `TYPE, CRTC_ID, FB_ID, SRC_X/Y/W/H, CRTC_X/Y/W/H, FB_DAMAGE_CLIPS` | ✅ complete |
  | CRTC 1 | `MODE_ID`, `ACTIVE` | `ACTIVE`, `MODE_ID` | ✅ complete |
  | Connector 1 | `CRTC_ID`, **`DPMS`** | `CRTC_ID` only | ❌ **one missing property kills the whole enumeration** |

  `wsi_display_get_connector` returns NULL and `wsi_get_connectors` turns that into
  `VK_ERROR_OUT_OF_HOST_MEMORY` (`:844-847`) **[M]**. So `vkGetPhysicalDeviceDisplayPropertiesKHR`
  fails outright, before anything interesting happens. *A `DPMS` enum property is a ~15-line
  addition* — cheap, and worth knowing, but it only moves the failure downstream.
- **Presentation uses only ioctls we already have.** There is **no `drmModePageFlip` and no
  `drmModeSetCrtc` anywhere in `wsi_common_display.c`** — `drm_atomic_commit` (`:2683-2779`) is
  the sole mechanism, using `MODE_ATOMIC` (+`ALLOW_MODESET`, `PAGE_FLIP_EVENT`, `NONBLOCK`) and
  `CREATEPROPBLOB` **[M]**. We implement both (`drm_device_interface.rs:1453-1458`) **[M]**, and
  we queue a flip event on `DRM_MODE_PAGE_FLIP_EVENT` (`:2496`) readable via `drm_read_events`
  (`:1332`) **[M]** — which is exactly what `drmHandleEvent` on the display fd wants (`:1898-1911`).
- **`drmGetDevice2` PCI matching should succeed.** `vkAcquireDrmDisplayEXT` gates on
  `wsi_device_matches_drm_fd`, which compares `VkPhysicalDevicePCIBusInfoPropertiesEXT` against
  `drmGetDevice2(fd)` and only ever matches `DRM_BUS_PCI` **[M]**. Venus deliberately reports
  the **guest virtgpu's** PCI info, not the host GPU's — `VN_SET_VK_PROPS(props,
  &renderer_info->pci.props)` (`vn_physical_device.c:843-845`), with the comment at `:1187-1196`
  saying `EXT_pci_bus_info` exists precisely so common WSI can do this comparison **[M]**. And
  we synthesize `/sys/dev/char/226:{0,128}/device/{vendor,device,subsystem,uevent,config}` with
  a real `PCI_SLOT_NAME` (`mkfs-f2fs-populated.py:568-588`) **[M]**. **[I]** that this matches.
- **DRM master is not a blocker, for an uncomfortable reason.** `local_drmIsMaster` is
  `drmAuthMagic(fd, 0) != -EACCES` (`wsi_common_display.c:3121-3137`) **[M]**, and we answer
  `DRM_IOCTL_AUTH_MAGIC => Ok(0)` unconditionally (`drm_device_interface.rs:1442`) **[M]** — so
  *every* fd, including `renderD128`, passes the master test. Combined with our
  `SET_MASTER`/`DROP_MASTER => Ok(0)` and the fact that `SETCRTC`/`PAGE_FLIP` never check master
  (`:1436-1439`, comment "Root single-seat: master is not gated") **[M]**, a Vulkan client would
  be *allowed* to drive the single CRTC while cosmic-comp is running, and the two would fight
  over one CRTC and one connector (`DRM_CRTC_ID = 1`, `DRM_CONNECTOR_ID = 1`) **[M]**.
  **Any direct-KMS Vulkan demo must therefore run with COSMIC stopped.**

**Now the real blockers, in descending hardness:**

1. **dma-buf export (§3.0)** — `vkGetMemoryFdKHR` → `PRIME_HANDLE_TO_FD` → `EINVAL`. Nothing
   downstream runs.
2. **dma-buf import.** Even given an export, `wsi_display_image_init` does
   `drmPrimeFDToHandle(wsi->fd, image->dma_buf_fd, ...)` then
   `drmModeAddFB2WithModifiers` (`:1685-1713`) **[M]**. Our `PRIME_FD_TO_HANDLE` is a *lookup*,
   not an import: it only maps back an fd we previously exported from one of our own dumb
   buffers (`syscall.rs:6110-6122`, `vfs::dmabuf_handle_of`) **[M]**.
3. **A Venus image lives on the host GPU; our scanout source is a guest-backed resource.** Our
   `SETCRTC`/flip path drives `SET_SCANOUT` with `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`
   against guest-backed resources; there is no path making a `BLOB_MEM_HOST3D` resource the
   scanout source (`drm_device_interface.rs:3443-3490` distinguishes the two) **[I]**.
   Venus compounds this by setting `wsi_device.supports_scanout = false` (`vn_wsi.c:86`) **[M]**,
   which pushes the common WSI onto the linear **prime-blit** staging path whenever no modifiers
   are negotiated — and we advertise `DRM_CAP_ADDFB2_MODIFIERS => 0` **[M]** (`:2064`) and expose
   no `IN_FORMATS` plane property, so no modifiers will ever be negotiated.
4. **The fence-fd blocker of §3.1**, since acquire/present sync goes through `vkGetFenceFdKHR`.
5. Missing `CRTC_QUEUE_SEQUENCE` / `CRTC_GET_SEQUENCE` **[M]** (absent from the dispatch table
   at `:1410-1458`). *Correction to a natural first guess:* these are needed only for
   `VK_EXT_display_control`'s `vkRegisterDisplayEventEXT` (`:3975`) and
   `vkGetSwapchainCounterEXT` (`:4058`) — **not** for the core present path. Not a blocker.
6. Connector `DPMS`, per the table above.

**Verdict: do not pursue `VK_KHR_display`.** It buys nothing over §1's dumb-buffer blit — which
reaches the same physical screen through a path `drmsmoke` already proves — and costs a
guest-side dma-buf import/scanout subsystem.

### 3.4 `VK_KHR_wayland_surface` — the real M4, with one important correction

The Wayland WSI binds a **mutually exclusive** pair of globals, chosen by `wsi_device->sw`
(`wsi_common_wayland.c:1401-1457`) **[M]**:

- `wsi->sw == false` → binds `zwp_linux_dmabuf_v1` (v3+), **never** `wl_shm`.
- `wsi->sw == true`  → binds `wl_shm`, **never** `zwp_linux_dmabuf_v1`.

`wsi_wl_display_init` hard-fails with `VK_ERROR_SURFACE_LOST_KHR` if neither is present
(`:1545-1549`) **[M]**. Everything else (`wp_presentation`, `wp_fifo_manager_v1`,
`wp_commit_timing_manager_v1`, `wp_color_manager_v1`, `wp_tearing_control_manager_v1`,
`wp_linux_drm_syncobj_manager_v1`) is optional **[M]**.

**The correction:** the tempting "Mesa has an shm fallback, so this needs no dma-buf" is only
half true. Venus sets `sw_device = true` **only** when the host renderer lacks
`EXT_external_memory_dma_buf` or is NVIDIA proprietary (`vn_wsi.c:69-82`) **[M]**. On our host
(RADV) neither holds, so Venus is a hardware device, binds `zwp_linux_dmabuf_v1`, and lands
back on §3.0's export gap.

**But there is a documented override.** `wsi_device->sw` is also settable by
`MESA_VK_WSI_DEBUG=sw` (`wsi_common.c:87`) **[M]**. With it:

- `wsi_wl_surface_create_swapchain` takes the CPU-image branch (`:3545-3558`) **[M]**.
- If `EXT_external_memory_host` is available → `WSI_WL_BUFFER_GPU_SHM`: the image memory *is*
  the `wl_shm` pool (memfd + `mmap`, `wsi_wl_alloc_image_shm` `:3215-3230`, imported via
  `VkImportMemoryHostPointerInfoEXT`) — **zero copy, zero DRM** **[M]**.
- Otherwise → `WSI_WL_BUFFER_SHM_MEMCPY`: a `memcpy(image->shm_ptr, image->base.cpu_map, …)`
  per present (`:2999-3003`) **[M]**.
- Either way: **no dma-buf, no `PRIME_*`, no KMS.** The data flow is identical to §1's readback,
  plus a Wayland client.

We already ship a working dependency-free `wl_shm` client (`leandros-applet`) **[M]** (MEMORY,
M7w), and cosmic-comp certainly advertises `wl_shm`. So M4 = "`MESA_VK_WSI_DEBUG=sw` + a
Vulkan `wl_shm` client under COSMIC" is a genuinely plausible next milestone whose main
unknown is whether the acquire/present semaphore dance still routes through `vkGetFenceFdKHR`
(§3.1). Do the ~40-line eventfd change first and that unknown collapses.

Two caveats to record now so they are not rediscovered: with `MESA_VK_WSI_DEBUG=sw` the whole
Venus device is treated as software by the WSI layer only — rendering still happens on the host
GPU; and `wsi_common.c:268`'s `MESA_VK_WSI_HEADLESS_SWAPCHAIN=1` can redirect *any* surface to
the headless implementation, which is a useful A/B control **[M]**.

### 3.5 WSI recommendation

**M3: none — no surface, no swapchain.** The offscreen readback of §1 is the only Vulkan output
milestone reachable with zero kernel changes, and §3.0 is the measured reason.

**M4: `VK_KHR_wayland_surface` on the `wl_shm` path**, gated behind `MESA_VK_WSI_DEBUG=sw`,
after the §3.1 eventfd change.

**`VK_KHR_display`: no.** **`VK_EXT_headless_surface`: no.**

**The two kernel items that gate all WSI**, in the order they should be done:
1. `PRIME_HANDLE_TO_FD` for virtgpu blob handles (not just dumb buffers) — `syscall.rs:6049`.
2. zero-size execbuffer + `FENCE_FD_OUT` → signalled `eventfd` — `syscall.rs:6041` region.

---

## 4. `scripts/run-qemu.sh` — the Venus change

### What the script does today **[M]**

- aarch64 picks `virtio-gpu-gl-pci` with `-display default,gl=on` (`:161-168`) — GL, but
  **no `venus=on`, no `blob=on`, no `hostmem=`**, so virglrenderer never initialises the
  Venus capset.
- x86_64 prefers `virtio-vga` (`:170-179`) — chosen deliberately because it exposes VGA
  registers so OVMF gives Limine a GOP framebuffer. It has **no GL at all**.
- A headless-host block (`:186-197`) rewrites `GL_ARGS` to `-display egl-headless` when
  `$OS != Darwin` and neither `DISPLAY` nor `WAYLAND_DISPLAY` is set.
- `-nographic` is never used; the script uses `-serial mon:stdio`. The `-nographic`
  trap therefore applies to **callers** appending it via `QEMU_EXTRA_ARGS`, and to wave
  scripts — `-nographic` implies `-display none` and **silently wins over any `-display`
  earlier on the command line**, killing Venus with no error **[M]** (README.md:491-494,
  TODO.md:157).

### The change

Add a `--venus` flag (plus `LEANDROS_VENUS=1` for harnesses). Insert the block **after** the
existing headless-host block at `:186-197` so it wins, and make it:

1. **Refuse on macOS, loudly.** `if [ "$OS" = "Darwin" ]; then echo "❌ --venus needs a host EGL implementation; macOS has none. Use the Linux box."; exit 1; fi`
   This is the whole macOS-compatibility story: the default path is untouched, and `--venus`
   fails fast with the reason rather than booting a guest whose `venustest` mysteriously
   reports "host lacks VIRGL/BLOB/CONTEXT_INIT" **[M]** (TODO.md:159-161 records exactly that
   false alarm).
2. **Refuse if the device is absent.** `$QEMU_SYSTEM -device help | grep -q virtio-gpu-gl-pci`
   — the script already uses this idiom. Do **not** autodetect virglrenderer: there is no
   reliable probe, and silently degrading to a non-Venus device is precisely what wasted a
   wave before. An explicit flag that fails loudly is the right shape.
3. **Set, on both arches:**
   `GPU_DEV="virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G"` and
   `GL_ARGS=("-display" "egl-headless")`.
   Both device strings are already interpolated as `-device "$GPU_DEV"`, so a comma-separated
   property list needs no other change **[M]**.
4. **On x86_64, drop the `-vga none`** that the UEFI branch hardcodes (`:361`). The archived
   working x86_64 Venus command line has **no `-vga none`** **[M]**
   (`.../m2c-venus-linux-2026-08-06/venuswave_x86_64.out:1`): q35's default std-VGA gives
   OVMF/Limine its GOP, and `virtio-gpu-gl-pci` rides alongside as the Venus device. This is
   the *proven-to-boot* configuration; keep it as the default for `--venus`.
5. **Optional, and worth one experiment:** `virtio-vga-gl,venus=on,blob=on,hostmem=4G` as a
   single device — VGA-compatible (so it can be the *only* display device and still satisfy
   OVMF) **and** GL/Venus-capable. QEMU's `virtio_instance_init_common` aliases all of the
   child `VirtIOGPUGL` properties onto the PCI proxy, so `venus`/`blob`/`hostmem` should be
   accepted **[I] — never tested; `virtio-vga-gl` appears nowhere in either repo [M]**. This
   matters for **A+**: with one device, the kernel's framebuffer console, Limine's GOP and the
   Venus device are all the same head, so a QMP `screendump` captures the triangle without
   ambiguity. With the (4) layout there are two heads and the primary is the std-VGA one.

Reference command lines to match, verbatim from the passing wave **[M]**:

```
# x86_64 (KVM)
-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless
# aarch64 (TCG, -cpu max,lpa2=off)
-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless
```

No guest env vars are needed — the wave harness sets none and `vktest` passes **[M]**.

Also worth a one-line guard while in there: after assembling `QEMU_ARGS`, if
`" ${QEMU_EXTRA_ARGS[*]} "` contains `-nographic` and `--venus` is set, abort with the reason.
That converts the documented silent trap into a diagnosable error.

Size: ~35 lines. Verifiable only on the Linux box.

---

## 5. Sequencing, sizes, and risk

| Step | What | Size | Where it can be verified |
|---|---|---|---|
| 1 | `run-qemu.sh --venus` (§4) | S, ~35 lines | Syntax/flag-parsing locally; **Venus only on the Linux box** |
| 2 | `vkrender` subtests 0–2 (fill → compute → triangle) + readback assertions | **M**, ~600 lines C + 3 tiny shaders; new Alpine build script | Compiles in the container anywhere; **runs only on the Linux box** |
| 3 | Stage `vkrender` in `mkfs-f2fs-populated.py` | XS, 2 lines | Locally (image-build only, no QEMU) |
| 4 | `vkrender --present`: dumb-buffer blit + `SETCRTC` (§1 step 10) | S, ~80 lines lifted from `drmsmoke` | Linux box; capture via QMP `screendump` |
| 5 | Wave harness (clone `venuswave.py`) + archive logs/screenshot | S | Linux box |
| 6 | *(gates all WSI)* `PRIME_HANDLE_TO_FD` for virtgpu **blob** handles, not just dumb buffers (§3.0) | S–M, `kernel/src/syscall.rs:6049` + a blob→phys resolver in `drivers` | Kernel builds locally; behaviour only on the Linux box |
| 7 | *(gates all WSI)* zero-size execbuffer + `FENCE_FD_OUT` → `eventfd2(1)` (§3.1) | S, ~40 lines in `kernel/src/syscall.rs` | as above |
| 8 | *(M4)* `VK_KHR_wayland_surface` `wl_shm` client under `MESA_VK_WSI_DEBUG=sw`; Khronos loader if we want stock binaries | L | Linux box |

Steps 1–5 are M3 and are independent of 6–8. Do not interleave them: 6 and 7 are kernel
changes that would put the M3 evidence in doubt if they land in the same wave.

### The step most likely to fail, and why

**Step 2, at subtest 0 — the very first `vkQueueSubmit` + wait.**

Not because of the pipeline or the shaders, but because this is the first time the Venus ring
carries a submission that the guest must then *wait on*. Three specific failure modes, in
order of likelihood:

1. **A silent hang in `vn_ring`'s relax loop.** This lane has already been bitten twice by
   exactly this — 10 ms `CLOCK_MONOTONIC` granularity starving the ring notify throttle
   (`75b32e3`) and `nanosleep` truncating sub-tick sleeps to zero, making Mesa's watchdog fire
   200× early and `abort()` (`fb398c7`) **[M]**. Both are fixed, but both were *only*
   discovered under sustained ring traffic, and a real submission is an order of magnitude
   more traffic than enumeration. Under aarch64/TCG especially, expect to re-tune before
   blaming the GPU. **Mitigation:** subtest 0 is `vkCmdFillBuffer` with no shader and no
   render pass — if it hangs, the ring is the only suspect. Instrument with `VN_DEBUG=init,result`
   (`vn_common.h:108-119` **[M]**) and the existing `[UCK]`/serial tracing before touching
   anything.
2. **`vkAllocateMemory` on a `HOST_VISIBLE` type failing to map.** We have proven blob mapping
   only for *Mesa's own* ring shmem. An app allocation takes the same path —
   `vn_MapMemory2` (`vn_device_memory.c:427-495`) lazily creates the bo with
   `VIRTGPU_BLOB_FLAG_USE_MAPPABLE` (set because `HOST_VISIBLE`,
   `vn_renderer_virtgpu.c:1139-1140`) then `DRM_IOCTL_VIRTGPU_MAP` + `mmap` (`:714`) **[M]** —
   but at app size and lifetime, against a 64 MiB `MAX_BLOB_BYTES` cap and 256
   `MAX_HOSTVIS_SPANS` **[M]** (`drm_device_interface.rs:3334`, `:893`). A 256×256 RGBA
   readback is 256 KiB, comfortably inside both.
   *This risk is lower than it first appears*, because Venus normalises the memory types for
   us: `vn_physical_device_init_memory_properties` (`vn_physical_device.c:947-988`) **strips
   `HOST_VISIBLE` from every non-coherent type** and **force-adds `HOST_CACHED` to the first
   coherent type** if none was cached **[M]** — so a `HOST_VISIBLE|HOST_COHERENT|HOST_CACHED`
   type is essentially always present and is the right one to pick. Note also Mesa's own
   comment at `vn_device_memory.c:439-450`: the first map **blocks** until the renderer
   injects the pages into the guest ("XXX … That is plain wrong"), so a slow first
   `vkMapMemory` under TCG is expected, not a hang.
   **Mitigation:** print every memory type's property flags and the chosen index before
   allocating; make the choice a logged decision, not an assumption.
3. **An unhandled `EXECBUFFER` field.** The driver logs
   `EXECBUFFER: fields asked for but NOT honoured` once per distinct shape **[M]** — grep the
   serial log for it on the first run. `sim_submit` passes `bo_handles`/`num_bo_handles`
   (now implemented **[M]**) and `ring_idx` (implemented **[M]**), so the expected answer is
   silence; anything printed is the bug.

Second-riskiest: **step 4**, because it is the only step whose success criterion is a
*picture* and therefore depends on `-display egl-headless` still serving QMP `screendump`
**[I]**. Guard against it by making step 2's guest-side pixel checksum the authoritative
proof and treating the screenshot as corroboration; if `screendump` under `egl-headless`
turns out to be empty, add `-vnc :0` rather than abandoning the milestone.

### Mac vs Linux box

- **Locally on the Mac:** all source/design work; the Alpine container build of `vkrender`
  (it is a cross-arch container build, not a host-EGL thing); the `run-qemu.sh` edit;
  `mkfs-f2fs-populated.py` staging; kernel compilation for step 6. Also: a non-Venus
  `run-qemu.sh` regression run to prove the flag changed nothing by default.
- **Only on the Linux box (`forain@172.16.158.150`, EndeavourOS, virglrenderer 1.3.0,
  QEMU 11.0.1 **[M]**):** every single thing that touches Venus. macOS has no EGL, so
  `virtio-gpu-gl-pci,venus=on` cannot initialise there — this is a host-platform fact, not a
  code defect **[M]** (TODO.md:159-161).

---

## 6. Open questions worth one experiment each

1. Does `virtio-vga-gl` accept `venus=on,blob=on,hostmem=4G`? (§4 item 5.) One QEMU launch
   answers it, and a yes makes the x86_64 story a single device.
2. Does QMP `screendump` produce a real image under `-display egl-headless`? (§5.)
3. Which RADV memory type index actually maps through our blob path? (§5 risk 2.) Answered
   for free by `vkrender`'s logging.
4. After steps 6 and 7, does `sim_syncobj_create` succeed, does `vkGetMemoryFdKHR` return a
   real fd, and does `vkCreateSwapchainKHR` light up? That is the gate on M4's WSI.
5. Does `MESA_VK_WSI_DEBUG=sw` actually flip Venus onto the `wl_shm` path, and does the guest
   have `EXT_external_memory_host` (zero-copy) or fall back to memcpy-per-present? (§3.4.)
   Answerable with a one-line env change once a Wayland Vulkan client exists.
6. Is Venus's advertised `VK_KHR_swapchain` gate
   (`renderer_sync_fd.semaphore_importable`, `vn_physical_device.c:1176`) satisfied on our
   host? Cheap to answer: have `vkrender` enumerate *device* extensions and log whether
   `VK_KHR_swapchain` is present. Worth adding to M3 even though M3 does not use it — it
   costs ten lines and pre-answers the first M4 question.
