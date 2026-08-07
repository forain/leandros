# M3 Progress — kmscube Mesa userspace NULL deref

Owner: deep-reasoner M3 wave agent (exclusive git+QEMU+images).
Started 2026-07-22. Resumes from K4 (main @ aa7ad8b, tree clean).

## Goal (M3 exit)
kmscube renders animated frames on /dev/dri/card0, screenshot-verified (2 shots ~1s apart
proving animation), BOTH arches, from FRESH f2fs image, release kernel builds only.

## Crash signature (from K4 notes, to reproduce+symbolize)
EL0, FAR=0x0, DFSC=6 translation fault, ESR EC=0x24, ELR ~ gallium_base + 0x..BE58, x0=1 x1=0.
Fires right AFTER dumb-buffer mmap returns OK. Kernel exonerated (drmsmoke 17/17 + gradient).
softpipe crashes EARLIER than kms_swrast. Null func-ptr call consistent with FAR=0 + EC=0x24.

## STATE / LOG
- (start) Read K4 notes + m3-gl-stack NOTES. Setting up.

## ROOT CAUSE (symbolized + traced) 2026-07-22
- Reproduced aarch64: ELR=0x40F34E58, libgallium base=0x4062F000 (17MB span mmap), offset=0x905E58.
- addr2line -> dri2_allocate_textures (gallium/frontends/dri/dri2.c:280): `images.back->texture`
  with images.back==NULL (FAR=0, texture at offset 0). image_mask has BACK set but back ptr NULL.
- Trace: platform_drm.c dri2_drm_image_get_buffers:335-336 sets image_mask=BACK, back=bo->image.
  bo came from gbm_dri_bo_create create_dumb fallback (gbm_dri.c:902 `if !dri->has_dmabuf_export`)
  which calloc's bo and NEVER sets bo->image -> back=NULL.
- has_dmabuf_export set from pscreen->caps.dmabuf & DRM_PRIME_CAP_EXPORT (gbm_dri.c:1246).
- Build is SOFTPIPE-ONLY (no llvmpipe; LLVM absent in zig cross-build). Softpipe never sets
  caps->dmabuf; default in util/u_screen.c:135-141 = drmGetCap(fd, DRM_CAP_PRIME) via get_screen_fd
  (softpipe_screen_get_fd -> kms-dri winsys get_fd = our card0 fd).
- KERNEL: drivers/src/drm_device_interface.rs std_handle_get_cap returns DRM_CAP_PRIME => 0.
  => caps->dmabuf=0 => has_dmabuf_export=false => create_dumb => bo->image NULL => crash.
- This is the arm drmsmoke never exercised (GET_CAP PRIME). NOT garbage, but under-reports a cap
  the softpipe+GBM software path REQUIRES. Legit kernel-side fix.

## FIX (candidate): report DRM_CAP_PRIME = IMPORT|EXPORT so softpipe takes the DRIimage path
  (gbm_bo_create -> dri_create_image_with_modifiers -> softpipe resource over kms-dri winsys dumb
  buffer, bo->image non-NULL). kmscube uses gbm_bo_get_handle (KMS handle), not PRIME fd, so
  PRIME_HANDLE_TO_FD not required for the flow. Testing empirically.

## FIX VERIFIED aarch64 2026-07-22
- drm_device_interface.rs: DRM_CAP_PRIME => IMPORT|EXPORT (was 0). Rebuilt aarch64.
- kmscube -D /dev/dri/card0: NO crash. EGL now advertises EGL_EXT_image_dma_buf_import +
  EGL_MESA_image_dma_buf_export. Render loop cycles two dumb buffers (double-buffered).
- 2 screenshots 1.2s apart: /tmp/m3-aarch64-frame1.png + frame2.png = spinning cube, ROTATED
  between frames = ANIMATING. aarch64 M3 EXIT MET.
- TODO: remove TEMP-M3-DEBUG MMAPLOG from syscall.rs; clean build BOTH arches + fresh images;
  verify x86_64; regression gate; commit; update plan doc.

## RESUME after session-limit kill 2026-07-22 (v2)
- Reconciled: git tree = only drivers/src/drm_device_interface.rs modified. MMAPLOG removal
  CONFIRMED clean (syscall.rs clean, no residue). Fix present (DRM_CAP_PRIME => IMPORT|EXPORT).
- Prior verified this session BEFORE kill: aarch64 + x86_64 kmscube ANIMATED (screenshots
  /tmp/m3-{aarch64,x86_64}-*.png), drmsmoke 17/17 both, scmtest 19/19, epolltest 8/8, evtest2
  8/8, idletest 0 both. The 3 vfstest FAILs (xattr_list f2fs+tmpfs, chroot_confines_symlink)
  were dirty-image residue from 4x repeated vfstest runs on one boot; fresh-image run = 0 FAIL.
- Redoing: clean build-all both arches -> fresh matched images -> final kmscube re-verify +
  authoritative regression on FRESH images -> commit -> update plan doc.

## RESUME v3 after 600s-watchdog kill 2026-07-22
- Killed: foreground driver.py cmd vfstest blocked >600s. Lesson (rule 1): the long QEMU
  interaction MUST be the run_in_background invocation itself.
- Clean both-arch RELEASE build DONE (11:07 images, all 6 fresh, exit 0). Fix intact, MMAPLOG gone.
- DEFINITIVELY confirmed vfstest 3-FAIL is same-boot repeated-run residue (NOT my change):
  first vfstest on fresh image = all 34 PASS incl chroot_confines_symlink_resolution,
  xattr_list_tmpfs, xattr_list_f2fs; only repeated same-boot runs fail them (xattr_list_tmpfs
  fails on tmpfs = same-boot state, proving residue). DRM GET_CAP change cannot touch xattr/chroot.
- Screenshots preserved to ~/code/leandros-artifacts/notes/m3-screenshots/ (aarch64-final1/2,
  x86_64-frame1/2) — spinning cube, rotated between frames, BOTH arches.
- Running /tmp/m3_final.sh backgrounded: fresh images both arches, single-connection marker
  capture (/tmp/m3_capture.py) for vfstest+drmsmoke+scmtest+epolltest+evtest2+idletest, +
  kmscube 2 screenshots per arch. Then: commit drivers/src/drm_device_interface.rs, update plan doc.

## M3 COMPLETE 2026-07-22 — COMMITTED
Commits on main:
  a13db0a drm: report DRM_CAP_PRIME so GBM softpipe takes the DRIimage path
  ba22bee docs: record M3 completion — kmscube renders animated frames both arches
Tree clean. Only drivers/src/drm_device_interface.rs changed (code) + wayland_cosmic_plan.md (doc).

### M3 EXIT EVIDENCE (fresh f2fs images, release build, both arches)
- kmscube renders the animated smooth-shaded cube on /dev/dri/card0; 2 screenshots/arch,
  cube rotated between them (frames differ). No crash; EGL advertises dma_buf import/export.
  Screenshots: ~/code/leandros-artifacts/notes/m3-screenshots/m3-{aarch64,x86_64}-final1.png + final2.png
- Regression (all 0 FAIL, both arches):
  aarch64: vfstest 34/34, drmsmoke 17/17, scmtest 19/19, epolltest 8/8, evtest2 8/8, idletest IDLE_CPU_US 0
  x86_64:  vfstest 34/34, drmsmoke 17/17, scmtest 19/19, epolltest 8/8, evtest2 8/8, idletest IDLE_CPU_US 0

### M4 landmines / notes
- PRIME_HANDLE_TO_FD / PRIME_FD_TO_HANDLE still return Unsupported (drm_device_interface.rs).
  kmscube's KMS-handle path doesn't need them, but anvil/cosmic linux-dmabuf or EGL dmabuf import
  WOULD. We now ADVERTISE EGL_EXT_image_dma_buf_import + EGL_MESA_image_dma_buf_export (because
  DRM_CAP_PRIME reports IMPORT). If M4 clients try dmabuf import/export they'll hit the Unsupported
  ioctls — implement PRIME handle<->fd then (real gem-handle <-> dmabuf fd), or gate the cap.
- Mesa build is SOFTPIRE-ONLY (no llvmpipe; LLVM absent from the zig cross-build). softpipe is slow
  under x86_64 TCG but renders. If perf matters for cosmic, an llvmpipe-enabled Mesa rebuild is the lever.
- Serial capture: driver.py/scmrun have a one-command output lag + scmrun's per-call drain discards
  mid-run output. For multi-test batches use a SINGLE persistent-connection capture with markers
  (/tmp/m3_capture.py pattern). vfstest xattr_list_*/chroot_confines_symlink FAIL on REPEATED
  same-boot runs (accumulated xattr/symlink residue incl. on tmpfs) — always first-run on a fresh image.
- The whole long QEMU interaction must BE the run_in_background invocation (600s stream watchdog).
