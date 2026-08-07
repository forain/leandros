# mesa-caps lane progress (host-only, repo READ-ONLY)

Task: capability matrix softpipe(current) vs llvmpipe(lane), gap closure, ship-swap recommendation.
Outputs -> ~/code/leandros-artifacts/notes/mesa-caps-matrix.md. Deliverable DONE.

## Evidence gathered
- Read llvmpipe-lane/NOTES.md + notes/llvmpipe-progress.md (build WORKS both arches, drop-in, smoke PASS).
- Read notes/kernel-readiness-audit.md (aarch64 SCTLR.UCI/UCT JIT blocker; x86_64 ready; image budget fits).
- Read notes/m5-progress.md (M5 blocker REDEFINED by that wave — see below).
- Mesa 25.3.6 source at llvmpipe-lane/src/mesa (== the built source). Traced the EGL dmabuf gating.

## DECISIVE source facts (Mesa 25.3.6)
- egl_dri2.c:688-695: EXT_image_dma_buf_import + _modifiers advertised together iff
  `has_dmabuf_import` = (pscreen->caps.dmabuf & DRM_PRIME_CAP_IMPORT). KHR_image_base ALWAYS on.
  MESA_image_dma_buf_export iff EXPORT bit. Whole block is `#ifdef HAVE_LIBDRM` (both builds have it).
- softpipe (sp_screen.c): sets get_screen_fd -> caps.dmabuf comes ONLY from u_screen.c:135-142
  drmGetCap(fd, DRM_CAP_PRIME). => KERNEL-dependent. NO modifier callbacks at all.
- llvmpipe (lp_screen.c:198-209): caps.dmabuf = IMPORT|EXPORT UNCONDITIONALLY when winsys->get_fd
  (the GBM/kms-dri case). Deviceless: udmabuf branch -> IMPORT even w/o /dev/udmabuf (line 205).
  lp_texture.c:1760-1826: FULL modifier interface (query/is_supported/create_with_modifiers = LINEAR).
- Kernel (drm_device_interface.rs:1114-1132, committed a7754d0): std GET_CAP reports
  DRM_CAP_PRIME => IMPORT|EXPORT *specifically to satisfy softpipe* (comment says so).
  DRM_CAP_ADDFB2_MODIFIERS => 0.
- Current ship set (m3-gl-stack) libgallium = SOFTPIPE ONLY (82MB, no llvmpipe strings).
  llvmpipe-lane libgallium = llvmpipe+softpipe (22MB). Both have gbm/dri_gbm.so (scanout parity).

## EMPIRICAL probe (caps_probe.c, surfaceless, Alpine) — aarch64 CONFIRMED
- softpipe: import=0 modifiers=0 KHR_image_base=1 export=0  RENDERER=softpipe
- llvmpipe: import=1 modifiers=1 KHR_image_base=1 export=0  RENDERER=llvmpipe(LLVM 19.1.4,128b)
  => matches source exactly. llvmpipe advertises import+modifiers even deviceless (udmabuf IMPORT).
- x86_64 probes: backgrounded (bxzxdhy66), source predicts identical (256-bit renderer).

## M5 reframing (from notes/m5-progress.md, the tree wave's own findings)
- anvil on CURRENT softpipe gets FULL dmabuf import+modifiers+export on the GBM-over-card0 display
  (m5-progress.md:325) => the EGL EXTENSION gap is NOT the on-target blocker on the GBM path.
- "Dmabuf import extension not available" = cosmic-comp fell to EGL_MESA_device_software renderer
  (is_software filter, mod.rs:272) = deviceless EGL. Fix = COSMIC_RENDER_DEVICE=226:0 (userspace env).
- Real remaining blocker = KERNEL/VFS dmabuf tmpfs-pool exhaustion (128 MAX_TMP_FILES) + INSTALL_FAIL
  fd leak, triggered by cosmic-comp reallocating scanout buffer EVERY frame. M5d wave owns this.
- gbm_bo_create_with_modifiers returns NULL on softpipe (no modifier cbs); llvmpipe would succeed(LINEAR).

## RECOMMENDATION = SWAP-AFTER-M5 (+ optional x86_64 llvmpipe experiment). See matrix doc.
DONE. Deliverable written: notes/mesa-caps-matrix.md.
