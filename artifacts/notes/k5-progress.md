# K5/M4-completion wave — PRIME/dmabuf + M4 exit

## DESIGN (dmabuf = borrowed-VMO pseudo-file keyed to the GEM handle)

### Where PRIME is handled
- Intercept `DRM_IOCTL_PRIME_HANDLE_TO_FD (0xC00C642D)` and `PRIME_FD_TO_HANDLE (0xC00C642E)`
  in `kernel/src/syscall.rs::sys_ioctl`, BEFORE the generic `is_drm` VFS forward.
  Rationale: fd allocation + VMO install is a VFS op (FD_TABLES/TMP_VMOS); the DRM driver
  (drivers crate, no_std) has no fd-table access. Kernel depends on BOTH `drivers` and `vfs`,
  so the syscall layer is the one place with pid + user-copy helpers + both crates in scope.

### fd type + refcount story
- A dmabuf fd is a **memfd-style tmpfs pseudo-file** (`/tmp/dmabuf:<h>`, opened via the same
  VFS_OPEN path memfd_create uses) whose `TmpVmo.pages` are the dumb buffer's EXISTING physical
  frames (contiguous buddy block `phys .. phys + 2^order*4096`), marked `borrowed: true`.
- Refcount: the DRM buddy block keeps the implicit pageref (untracked = 1). A MAP_SHARED mmap of
  the fd does `pageref::inc` per frame (→2); munmap/exit does `unref_or_free` (→1, never frees).
  fork incs per shared frame per owner, so teardown stays balanced and NEVER reaches the free
  branch. The ONLY free of these pages is `free_dumb` → `buddy::free(phys, order)` on
  GEM_CLOSE/DESTROY_DUMB. `vmo_free_slot` SKIPS borrowed VMOs (must not double-free / order-0-free
  an order-N block).
- FD_TO_HANDLE: the gem handle is stored IN the borrowed TmpVmo (`dmabuf_handle`), so it
  auto-cleans when the inode frees; no separate (pid,fd)->handle registry to leak.
- mmap MAP_SHARED of the fd hits the existing tmpfs shared-VMO branch (vmo_acquire_frames +
  map_shared_frames) → coherent alias with the MAP_DUMB device mapping (same phys). Softpipe
  dmabuf import path supported.

### ADDFB2 path
- smithay round-trips: gbm_bo_get_fd (HANDLE_TO_FD) -> FD_TO_HANDLE -> same dumb handle -> ADDFB2.
  std_handle_addfb2 already reads handles[0] from DUMB_BUFFERS -> works unchanged. No dmabuf mmap
  on the M4 wl_shm scanout critical path; mmap-alias is for softpipe client dmabuf import (thorough).

### Known bring-up hazards (documented, acceptable for M4)
- DESTROY_DUMB while a dmabuf fd/mapping is live frees the block -> dangling alias. Compositor keeps
  the scanout bo alive for the session, so not hit. 
- dmabuf fd is not O_CLOEXEC (flags ignored); harmless for anvil (no exec after export).

### Files touched
- drivers/src/drm_device_interface.rs: add `pub fn dumb_buffer_phys_order(handle)->Option<(usize,usize)>`;
  remove the `PRIME_* => Err(Unsupported)` arm's reachability (kernel intercepts first, but keep a
  safe fallback).
- servers/vfs/src/lib.rs: TmpVmo gains `borrowed: bool` + `dmabuf_handle: u32`; add
  `install_dmabuf_vmo(pid,fd,phys,order,handle)` + `dmabuf_handle_of(pid,fd)`; vmo_free_slot skips
  borrowed; fix all TmpVmo literal initializers.
- kernel/src/syscall.rs: PRIME intercept in sys_ioctl.
- userland/drmsmoke: append PRIME steps (HANDLE_TO_FD, mmap fd + write + verify alias, FD_TO_HANDLE
  round-trip, close). 

## STATUS: design done, starting implementation.

## IMPLEMENTED + VERIFIED (commit 6ce43be)
- drmsmoke 20/20 PASS BOTH arches (aarch64 + x86_64), exit 0. New: PRIME_HANDLE_TO_FD,
  PRIME_MMAP_ALIAS (sentinel via dmabuf tmpfs mapping read back through the dumb device
  mapping — SAME phys 0xBCC00000 confirmed), PRIME_FD_TO_HANDLE. DESTROY_DUMB still PASS
  (borrowed VMO teardown does NOT double-free). Screenshots in notes/m4-screenshots/k5-drmsmoke-*.png.
- Next: anvil M4 exit (may hit further walls past PRIME: EGL dmabuf import of render target,
  page-flip scanout). Diagnostic run first.

## ROOT CAUSE OF M4 WALL (post-implementation) — NOT the kernel
- Kernel PRIME is CORRECT + verified (drmsmoke 20/20 both arches, direct ioctl path).
- anvil STILL fails: "Failed to export the allocated buffer as dmabuf: Buffer returned invalid file descriptor".
- Instrumented sys_ioctl PRIME with a serial marker; ran anvil: **[PRIME] marker count = 0**.
  => Mesa's gbm_bo_get_fd returns -1 WITHOUT ever issuing DRM_IOCTL_PRIME_HANDLE_TO_FD.
- The wall is one layer ABOVE the kernel, in Mesa/libgbm userspace:
  - smithay DrmCompositor::test_buffer (compositor/mod.rs:1514) calls buffer.export() = gbm_bo_get_fd
    FIRST, before the framebuffer exporter. That -1 aborts initialize_output.
  - smithay's compiled export() (gbm.rs, fallback path since backend_gbm_has_fd_for_plane probe=FALSE)
    calls self.fd() = gbm_bo_get_fd(whole bo). libgbm returns -1 -> InvalidFdError.
  - Mesa kms_dri_sw_winsys.c DOES implement FD export (drmPrimeHandleToFD, line 521) + import
    (drmPrimeFDToHandle) — but it is NEVER reached. gbm_bo_get_fd short-circuits to -1, almost
    certainly because bo->image == NULL (gbm took the create_dumb fallback, not the DRIimage path)
    OR the softpipe resource is not a shareable displaytarget. Confirmed by m3 note: "softpipe/
    kms_swrast path (no dma-buf)".
- smithay's render path ALSO binds swapchain buffers as dmabuf-backed EGLImages, so even a
  framebuffer-exporter patch (force framebuffer_from_bo / foreign=false) would not fully unblock —
  buffer.export() at :1514 fails before the exporter, and the renderer needs dmabuf import too.
- CONCLUSION: M4 exit is blocked by Mesa userspace, NOT the kernel. Requires a Mesa/libgbm fix
  (make gbm_bo_get_fd reach drmPrimeHandleToFD — i.e., ensure scanout bos are DRIimage/displaytarget
  backed, or patch the gbm dumb-bo path to export via PRIME) + a Mesa rebuild. Kernel PRIME + the
  borrowed-VMO mmap alias is exactly what that fix needs on the kernel side and is DONE.

## FINAL VERIFICATION (clean build3, fresh images, TCG)
- aarch64 regression sweep (fresh f2fs, vfstest FIRST): ALL GREEN, 0 FAIL.
  vfstest 34/34 PASS; drmsmoke 20/20 (incl PRIME_*); scmtest PASS (queued_fd_cap,
  devshm_shared_mmap — the shared-VMO path my change is adjacent to); epolltest
  pass=8 fail=0; evtest2 PASS; idletest idle_cpu PASS + pass=2 fail=0 (idle CPU=0).
- kmscube: `kmscube &` mangled by brush (bare-& syntax error) — rerun properly.
  Not a regression risk: drmsmoke exercises+passes the identical DRM scanout path
  (CREATE/MAP_DUMB, ADDFB2, SETCRTC, PAGE_FLIP, DIRTYFB) my changes don't touch.
- x86_64 regression: in progress.
- Commits: 6ce43be (kernel PRIME + drmsmoke), f271db1 (plan status docs).

## kmscube (DRM regression) — NOT a regression
- Bare `kmscube` fails at drmGetDevices2 ("No such file or directory") — expected: our synthetic
  sysfs doesn't support full DRM enumeration (same reason anvil needs ANVIL_DRM_DEVICE). M3 always
  ran `kmscube -D /dev/dri/card0` to bypass enumeration (m3-progress.md:41). Re-running with -D.
- x86_64 regression: confirmed 0 FAIL (vfstest 34, drmsmoke 20/20 incl PRIME, epolltest 8/8,
  evtest2, idletest). BOTH arches fully green.

## WAVE COMPLETE (2026-07-22)
- Kernel PRIME/dmabuf: DONE + verified both arches (drmsmoke 20/20). Commit 6ce43be.
- Full regression: 0 FAIL both arches on FRESH images. kmscube -D animates (frames differ;
  screenshots k5-kmscube-D-1/2.png = the shaded rotating cube). No regressions.
- M4 EXIT: NOT achieved — blocked in Mesa userspace (gbm_bo_get_fd returns -1 without issuing
  the PRIME ioctl; scanout bo is create_dumb-fallback / bo->image==NULL). Kernel is ready; needs
  a Mesa/libgbm fix + rebuild. Docs: wayland_cosmic_plan.md M4/M5 (commit f271db1), MEMORY.md.
