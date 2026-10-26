# lane/kmscube2 2026-10-26 (openbugs L11.8) - FIXED: kmscube renders on the v3d stub (QEMU aarch64)

Reproduced without a Pi: `drmsmoke --arm-v3d` + kmscube on QEMU aarch64 (HVF), release build.

## 1. FAR=0x10 = a Mesa BUILD artifact, not kernel/musl/relibc
- Repro with the 2026-08-20 out-of-tree v3d-only build (mesa-v3d-rpi5/out/stage-aarch64) swapped in via
  LEANDROS_GPU_STAGE: `[EXC] FAR=0x10 ELR=0x40E76548 x0=sp+7 x1=0x10`, pc in libgallium text VMA
  [0x40a41000, file+0x90f000) -> load base 0x40122000 -> vaddr 0xD54548.
- llvm-symbolizer: `mesa_bytes_to_hex` (util/hex.h:40) inlined in `_mesa_sha1_format` (mesa-sha1.c:46),
  called from v3d_disk_cache_init: `build_id_data(build_id_find_nhdr_for_addr(...))` with a NULL note
  -> NULL + 0x10. That libgallium has NO NT_GNU_BUILD_ID note (llvm-readelf -n: none); the shipped
  megadriver (m3-gl-stack/gpu-stage-aarch64, build-gpu-stack-alpine.sh DRIVERS=zink,virgl,softpipe,v3d)
  has one and already contains v3d. With the shipped stage the fault is gone ("V3D 7.1.6.4").
  Rule: never ship a Mesa linked without --build-id (any disk-cache driver dereferences it).

## 2. Mesa picks virtio_gpu from the PCI id, not DRM_IOCTL_VERSION (QEMU only)
card0's sysfs device is PCI 1AF4:1050, and Mesa's loader maps PCI ids before asking VERSION, so an
armed stub still loads "virtio_gpu" (then kms_swrast). Not a Pi problem (platform device). Tests use
MESA_LOADER_DRIVER_OVERRIDE=v3d.

## 3. Two kernel gaps after screen creation (fixed)
- `SYNCOBJ_HANDLE_TO_FD`+EXPORT_SYNC_FILE fell through to ENOSYS: sync_file::enabled() was on only for
  gpu_uapi backends. v3d_fence_create exports out_sync on every flush; failure = no fence = no front
  buffer ("Failed to get a new framebuffer BO"). Now also on while v3d is armed.
- ADDFB2 refused v3d BOs (blob_framebuffer skips v3d_va != 0, dumb lookup misses -> ENOENT). New
  v3d_framebuffer(): linear, pitch == width*4, offset 0 -> host 2D resource over the BO's contiguous
  pages, one per BO (V3D_SCANOUT_RES), unref'd in blob_unref when the BO dies.
Result: `Rendered 29 frames ... (98 fps)` on V3D, SUBMIT_CL/WAIT_BO exercised; disarm -> softpipe ok.

## Guard
runtests regress: aarch64-only `drmsmoke --arm-v3d && MESA_LOADER_DRIVER_OVERRIDE=v3d kmscube -c 30`
(always disarms). On the unfixed tree the same command exits 255 (kmsc5). Regress aarch64 49/49 PASS.

## Open
- Stub SUBMIT_CL renders nothing (no GPU): frames are not pixels. A real Pi needs a real V3D driver
  (submit CL to the V3D 7.1 core, MMU page tables at v3d_va, IRQ-driven fence signalling) and its
  scanout must accept the tight v3d stride.
- Optional: packer check that every staged libgallium has a GNU build-id note.
