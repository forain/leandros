# lane/nvzink — 2026-10-08 — M7: OpenGL ES via zink on NVK (GBM/EGL)

Branch `lane/nvzink`, rebased onto main `2d85a67` (lane/nvkms merged). Not
pushed. Goal: GLES through GBM + EGL on the RTX 3070 the way cosmic-comp
(smithay GLES renderer on EGL/GBM over card0) and Firefox (EGL/GLES on
Wayland, dma-bufs) use it: Mesa's zink on NVK on the nouveau UAPI layer.
Mesa stays unmodified 25.3.6; the GPU stage was **not rebuilt** (it already
had zink + EGL + GBM + GLES next to NVK).

## 1. How the loader picks zink (Mesa 25.3.6 source)

- `src/loader/pci_id_driver_map.h`: vendor 0x10de maps to `nouveau` and to
  `zink`, both behind `nouveau_zink_predicate()` (`src/loader/loader.c`).
- The predicate (only for a DRM device named `nouveau`) prefers zink when
  `NOUVEAU_GETPARAM_HAS_VMA_TILEMODE` == 1 **and** `CHIPSET_ID` >= 0x160
  (Turing+); it requires zink from 0x1a0. `NOUVEAU_USE_ZINK` overrides.
- Our layer answered HAS_VMA_TILEMODE = 0, so the loader chose `nouveau` GL,
  which is not built; GBM/EGL then retry an *inferred* zink
  (`dri_screen_create` in gbm_dri.c). It would have worked, by accident.
- Now `uapi.rs` answers **HAS_VMA_TILEMODE = 1**. That is honest: GEM_NEW
  keeps `tile_mode` / PTE kind and GEM_INFO returns them (Linux 6.10's
  meaning: tile mode and kind accepted on allocations under VM_BIND). The
  GPU-side kind still comes from each VM_BIND. Result: zink is picked
  explicitly, no environment variable at all; and NVK now advertises
  **VK_EXT_image_drm_format_modifier** (`has_alloc_tiled`), which zink needs
  for dma-buf modifiers (`query_dmabuf_modifiers`, EGL
  `EXT_image_dma_buf_import_modifiers`, GBM `create_with_modifiers`).
  nvktest is unaffected (no DRM-modifier images).
- zink then matches the Vulkan device by `stat(renderD128).st_rdev`
  (226:128) against NVK's `VkPhysicalDeviceDrmPropertiesEXT`; our VFS
  already reports st_rdev.

## 2. What zink needs from the kernel that nvktest did not exercise

| need | before | now |
|---|---|---|
| DRM modifiers (VK_EXT_image_drm_format_modifier) | not advertised by NVK | HAS_VMA_TILEMODE=1 |
| vkGetSemaphoreFdKHR / vkImportSemaphoreFdKHR (SYNC_FD) = SYNCOBJ_HANDLE_TO_FD / FD_TO_HANDLE with sync-file flags | ENOSYS | done |
| implicit sync on shared images: DMA_BUF_IOCTL_EXPORT/IMPORT_SYNC_FILE (zink_batch.c, zink_synchronization.cpp; NVK's WSI too) | ENOTTY | done (per-object reservation) |
| EGL_ANDROID_native_fence_sync (smithay's render fence) | advertised (zink cap), export failed | works (sync file) |
| SYNC_IOC_MERGE / FILE_INFO / SET_DEADLINE (vk runtime merge, sync_valid_fd) | ENOTTY | done |
| DMA_BUF_IOCTL_SYNC (CPU access brackets) | ENOTTY | 0 (coherent memory) |
| PRIME of non-power-of-two sysmem | token-only (no frames) | frames for any page count |
| PRIME of VRAM | token-only | unchanged: FD_TO_HANDLE works (all zink/GBM/EGL paths), mmap of the dma-buf fd does not (needs BAR1 windows) |
| timeline syncobjs, GBM bo creation, gbm_bo_map | (nvuapi) / userspace | zink maps VRAM bos through a staging copy, no kernel need |
| opaque syncobj fds, SYNCOBJ_EVENTFD | ENOSYS | unchanged (needed only for wp_linux_drm_syncobj explicit sync, which cosmic-comp then does not advertise) |
| dma-buf poll() honouring implicit fences | always ready | unchanged (smithay's blocker does not wait; zink orders through the sync-file ioctls instead) |

### Design (drivers/src/sync_file.rs, sync_file_core.rs, kernel/src/syncfile_ioctl.rs)

- A **sync file is an eventfd** (same object as the virtgpu out-fence):
  counter 0 until the fence retires, then 1, so poll(POLLIN) has sync-file
  semantics. The VFS notes a never-reused **token** on it
  (`EVFD_ORIGIN_SYNC_FILE`); `sync_file` maps token -> `Fence` only while
  pending. A token without an entry is signalled — a fence never
  un-signals — so no VFS close hook is needed. `drm_tick` signals retired
  ones (try-locks, one relaxed load when idle). GPU waits on an imported
  sync file go through the syncobj and the backend's in-fence path, not the
  eventfd.
- **dma-buf reservation**: `BTreeMap<bo obj, Resv{write, read}>` of pending
  fence ids; IMPORT adds (WRITE flag -> writer), EXPORT returns writers for a
  reader, everything for a writer, a signalled stub when empty; entries die
  with their last pending fence.
- The fd-minting ioctls are answered in the syscall layer (it owns the fd
  table, as for PRIME), in a separate file `kernel/src/syncfile_ioctl.rs`
  with a one-line hook in `sys_ioctl`.
- **Gate**: answered only while a GPU UAPI backend is registered (the NVIDIA
  card) or with `drm.syncfile=1` on the kernel command line (test switch).
  Otherwise the old answers (ENOSYS / ENOTTY): Mesa's WSI probes these, and
  the virtio/Venus/virgl desktop must not change strategy.
- Pure semantics host-tested (`drivers/uapi-tests`: 20/20, 5 new).

## 3. nvgltest (ports/nvk-tests/nvgltest.c)

Built by `ports/nvk-tests/build.sh` against the GPU Mesa stage (headers +
libEGL/libGLESv2/libgbm), staged as `/bin/nvgltest` by mkfs.
`nvk-autotest` runs it after nvktest with `VK_ICD_FILENAMES` pinned to NVK,
then again on `/dev/dri/card0` (`--only=info,fbo,gbm_bo,scanout,prime`), then
vulkaninfo, then nvkmstest (last, as lane nvkms wants). No eglinfo/es2_info
exist in the stage (mesa-demos); `info` prints the equivalent.

| subtest | what |
|---|---|
| info | DRM name/caps, HAS_VMA_TILEMODE + chipset -> loader prediction, GBM backend, EGL vendor/version/APIs/extensions/device node, GLES 3.2->3.0->2.0 surfaceless context, GL strings + extensions. On nouveau the renderer must contain `zink` (software = FAIL) |
| fbo | nvktest's triangle in GL: 256x256 RGBA8, 13 named pixels, red 18432 +/- 600, FNV-1a hashed top-down (same as nvktest: **0x02C0FDC5** on virgl and on NVK expected), ASCII art, 32x32 PPM base64 |
| gbm_bo | LINEAR ABGR8888 gbm_bo rendered through an EGLImage (NATIVE_PIXMAP), glReadPixels + gbm_bo_map must equal fbo |
| scanout | **what cosmic-comp allocates for the NVIDIA KMS**: gbm_bo_create_with_modifiers2(XRGB8888, {LINEAR}, SCANOUT\|RENDERING), rendered, read back both ways, dma-buf export |
| gbm_tiled | the driver's own (block-linear) modifiers from eglQueryDmaBufModifiersEXT |
| prime | gbm_bo -> dma-buf; re-import as EGLImage(LINUX_DMA_BUF, + modifier) sampled into another FBO, and as gbm_bo_import(FD_MODIFIER) + gbm_bo_map; fstat/lseek size; mmap probe (expected to fail for VRAM) |
| fence | EGL_KHR_fence_sync; EGL_ANDROID_native_fence_sync export -> poll -> re-import -> eglWaitSyncKHR; DMA_BUF_IOCTL_EXPORT_SYNC_FILE on the shared bo |
| throughput | 500 frames x 64 triangles at 1024x1024 |

Exit: failures, 126 on watchdog, 3 when the node is not nouveau (all SKIP);
`--any-driver` runs everything anyway.

## 4. LINEAR XRGB8888 scanout (question from lane nvkms)

KMS on the NVIDIA card0 lists only XR24/AR24 x LINEAR, so smithay allocates
scanout buffers with `gbm_bo_create_with_modifiers2(.., {LINEAR}, ..)`. On
NVK this needs **VK_EXT_image_drm_format_modifier**, which NVK only exposes
when HAS_VMA_TILEMODE = 1 — i.e. it needs this lane. With it, NVK's
`nvk_get_image_plane_format_features` gives LINEAR-modifier images
COLOR_ATTACHMENT (DRM-modifier tiling, not VK_IMAGE_TILING_LINEAR, which has
none) and renders them through a block-linear `linear_tiled_shadow`
(allocated with GEM_NEW tile_flags, copied to the linear image on the
foreign-queue release zink issues for dma-bufs). The linear image itself is
a VRAM GEM, which uapi_kms dups and blits from. Without this lane zink would
have no modifier support and would fall back to VK_IMAGE_TILING_LINEAR,
which NVK cannot render to. The `scanout` subtest checks exactly this on the
PC, on renderD128 and on card0.

## 5. Session scripts

`ports/greetd/data/gpu-env`: new "zink over NVK" branch for
`GPU_DRM_NAME=nouveau` (gpuprobe caps already reports the name; gpuprobe gl
already builds GBM/EGL on card0 the cosmic-comp way, so **gpuprobe.c needed
no change** and the Mesa stage no rebuild). No driver override is needed;
only `VK_ICD_FILENAMES` is pinned to the NVK manifest; clients get
`ICED_BACKEND=wgpu WGPU_BACKEND=vulkan` (NVK directly). On by default;
kernel command line `nvrm.desktop=0` turns it off, and `nvrm.vktest=1` (the
stick's autotest boot, whose nvkmstest needs DRM master on card0 and whose
log needs the text login) keeps it off unless `nvrm.desktop=1`.

## 6. Verification (Mac: aarch64 HVF, x86_64 TCG)

| check | result |
|---|---|
| `drivers/uapi-tests` host | 20/20 |
| regress, default build, after rebase on 2d85a67 | aarch64 **15/15**, x86_64 **15/15** (also 15/15 both before the rebase) |
| `runtests --suite drm --virgl` aarch64 | drmsmoke PASS (sync-file gate closed: one SKIP line) |
| `nvgltest --any-driver` aarch64 virgl (ANGLE/Vulkan, GLES 3.1) | 6 PASS 1 SKIP (gbm_tiled: virgl has no modifiers); fbo FNV-1a 0x02C0FDC5 = nvktest's; native fence ok; dma-buf sync file n/a (gate closed) |
| ffsession aarch64 virgl, with Firefox | greeter, panel, cosmic-term, 7 firefox processes (before and after the rebase) |
| x86_64 nvidia stick (`--cmdline drm.syncfile=1`), QEMU USB boot, no NVIDIA GPU | nvktest exit 3, **nvgltest 8 SKIP exit 3** ("render node driver is 'virtio_gpu'"), nvkmstest 4 PASS 2 SKIP, `nvk-autotest: done` |
| drmsmoke on that boot (gate open) | SYNC_FILE_EXPORT_SIGNALED_POLLIN, _EXPORT_NULL_FENCE_EINVAL, _IMPORT_SIGNALS_SYNCOBJ, _IMPORT_NOT_A_SYNC_FILE_EINVAL, SYNC_IOC_FILE_INFO, SYNC_IOC_MERGE, DMABUF_EXPORT_SYNC_FILE_IDLE, DMABUF_IMPORT_SYNC_FILE all PASS; rc 0 |

Not testable in QEMU: zink on NVK itself, pending (GPU) fences through sync
files, VRAM dma-bufs between processes, LINEAR shadow rendering.

## 7. On the PC

```sh
./scripts/nvidia-test-stick.sh /dev/diskN      # full build (kernel, userland, tests changed)
```
Boot, leave the keyboard alone; after nvktest, nvgltest runs (~10-30 s), then
the card0 pass, then nvkmstest. Then `sync; poweroff`, read the log, and
`grep -a 'nvgltest\|nvuapi.*ERROR\|Xid' nvrm-boot5.log`.

Expected:
```
[INFO] nouveau: HAS_VMA_TILEMODE=1 (rc 0), CHIPSET_ID=0x174 (rc 0) -> Mesa's loader picks zink (nouveau_zink_predicate)
[GL  ] GL_RENDERER: zink Vulkan 1.4(NVIDIA GeForce RTX 3070 (NVK GA104) (MESA_NVK))   (exact text may differ)
nvgltest info: PASS (GL_RENDERER="zink Vulkan 1.4(...NVK GA104...)", OpenGL ES 3.2)
nvgltest fbo: PASS (red=18432 px, other=0, 0/13 named pixels wrong, FNV-1a 0x02C0FDC5)
nvgltest gbm_bo: PASS (... == fbo; gbm_bo_map matches)
nvgltest scanout: PASS (XRGB8888 modifier 0x0 stride 1024, dma-buf ok; ...)
nvgltest gbm_tiled: PASS (modifier 0x300000000..., ...)
nvgltest prime: PASS (...; EGLImage re-import matches; gbm_bo_import+map matches)
nvgltest fence: PASS (fence_sync ok, native_fence ok, dmabuf sync_file ok)
nvgltest throughput: PASS (...)
==== nvgltest done: 8 passed, 0 failed, 0 skipped, 0 timeout ====
```
To try COSMIC on the card: `--cmdline "nvrm.vktest=0"` (or `nvrm.desktop=1`).

Decision tree:
1. `info: FAIL ... not zink` / software -> the loader line shows why
   (HAS_VMA_TILEMODE / chipset); `eglInitialize` failure -> zink found no
   matching Vulkan device (check the [EGL ] device node and NVK's
   `[nvuapi] open` lines).
2. `fbo` FAIL with right count, other checksum -> rasterisation detail, not
   a failure of the path (compare the ASCII art).
3. `gbm_bo`/`scanout` FAIL in gbm_bo_map only -> zink staging copy from VRAM;
   FAIL in glReadPixels -> EGLImage/FBO on the linear image (NVK shadow).
4. `prime` FAIL -> PRIME_HANDLE_TO_FD/FD_TO_HANDLE of a VRAM bo (`[nvuapi]
   PRIME export/import` lines).
5. `fence` native_fence EXPORT FAIL -> SYNCOBJ_HANDLE_TO_FD (look for the
   ioctl errno); POLL FAIL -> the fence never retired (watchdog lines).
6. Hang -> watchdog TIMEOUT line names the step.

## 8. Risks / known limits

- Nothing here has run on NVIDIA hardware; zink on NVK on this UAPI layer is
  first exercised by the next PC boot.
- Sync-file poll readiness is serviced by the 100 Hz DRM tick (<= 10 ms
  late); GPU-side ordering does not depend on it.
- In-fences from another process's channel are waited for on the CPU in EXEC
  (nvuapi design); with implicit sync on every shared image this serialises
  client and compositor submissions somewhat.
- zink exports a dma-buf (one PRIME export = one tmpfs node + VMO) and a
  sync file per exportable image per submit: per-frame fd churn.
- mmap of a VRAM dma-buf still fails (BAR1 not used for user mappings).
- `fence` treats a missing EGL_ANDROID_native_fence_sync as FAIL on nouveau.
