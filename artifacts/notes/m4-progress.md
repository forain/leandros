# M4 (anvil) wave progress

Mission: anvil composites a wl_shm xdg-shell client, cursor follows virtio-tablet,
keyboard reaches client. Screenshot-verified BOTH arches, fresh f2fs, release kernel.

## COMMITS (main)
- 994563e libudev shim: drop phantom event2 touchscreen
- 0bed5ad libseat shim: dispatch must not block on idle eventfd (ROOT CAUSE #1)
- 9284a00 mkfs: M4 ship set + recursive tree pack + synthetic /sys skeleton
- (PENDING) drivers/src/drm_device_interface.rs: OBJ_GETPROPERTIES stub + connector mode htotal/vtotal

## ROOT CAUSES FOUND + FIXED (anvil now reaches Output creation)
1. libseat shim dispatch blocked on EFD_NONBLOCK eventfd (kernel sys_eventfd2 ignores flags -> fd
   blocking -> sys_read EAGAIN-loops forever). FIX: shim dispatch returns 0 without reading. [committed]
2. /sys did not exist -> drm-rs is_device_drm stat -> panic. FIX: synthetic /sys/dev/char/226:0/
   device/drm/card0 dir skeleton in mkfs. [committed]
3. anvil rejects software EGL (is_software) + empty DRM udev enumeration. FIX: patched anvil/src/udev.rs
   (allow software + ANVIL_DRM_DEVICE direct device_added when enumeration empty). Rebuilt anvil
   (isolated build tree ~/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack/src/smithay; incremental 5s/arch;
   --no-default-features --features udev). Patch: notes/m4-anvil-udev-patch.md. [anvil binary, not repo]
4. OBJ_GETPROPERTIES ioctl returned EPERM -> smithay LegacyDrmDevice reset panicked. FIX: kernel stub
   returns count_props=0 (DPMS enum becomes no-op). [drm_device_interface.rs, PENDING commit]
5. Connector mode htotal/vtotal=0 -> smithay refresh calc divide-by-zero (output.rs:97). FIX: kernel
   populates htotal/vtotal/sync + consistent clock for 60Hz. [drm_device_interface.rs, PENDING commit]

## ANVIL STATUS (aarch64, m4-anvil-run4.log)
Full session init -> wayland-1 socket -> XKB "English (US)" -> event0/1 via libseat -> libinput ->
LegacyDrmDevice -> EGL/GLES2 (OpenGL ES 3.1 Mesa 25.3.6 softpipe) -> shaders -> "Trying to setup
connector HDMI-A-1 crtc=1" -> "Creating new Output HDMI-A-1". No panics through there. Foreground
`timeout` killed the render loop at window edge; need backgrounded/longer capture + screenshot.

## Launch env (guest): ANVIL_DRM_DEVICE=/dev/dri/card0 SMITHAY_USE_LEGACY=1 XDG_RUNTIME_DIR=/run/user/0
## wl client: WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/0 wlclient  (m4-client/wlclient-<arch>)
## QMP: LEANDROS_QEMU_EXTRA='-qmp unix:/tmp/leandros-qmp.sock,server,nowait'; m4-client/qmp.py move/click/key
## Harness: each QEMU interaction = ONE run_in_background; pkill -f qemu-system between; clean-slate prologue.

## ROOT CAUSE #6 (CURRENT BLOCKER, kernel DRM): no plane model -> anvil can't build scanout surface
- anvil creates the smithay Output + wl_output, then `drm_device.planes(crtc)` -> smithay's planes()
  calls plane_handles() = DRM_IOCTL_MODE_GETPLANERESOURCES -> kernel returns EPERM (unimplemented) ->
  anvil `warn!("Failed to query crtc planes")` + RETURN from connector_connected (no DrmOutput created).
  anvil's event loop still runs + accepts the wl client (socket listening), but no scanout surface ->
  never composites; client hangs in wl_display_roundtrip (globals not serviced without an output).
- Screenshot m4-aarch64-A-anvil.png = the serial console showing anvil's full init log ending at the
  plane WARN (anvil has NOT taken over the display).
- FIX NEEDED (bounded kernel DRM, = deferred K4 design §D compositor plane surface; blocks M5 too):
  implement GETPLANERESOURCES (list >=1 plane) + GETPLANE (possible_crtcs incl crtc0, formats e.g.
  XRGB8888) + OBJ_GETPROPERTIES-for-planes returning a "type"=Primary(1) property + GETPROPERTY(type)
  returning name "type" (enum). smithay plane_type() hits unreachable!() if the "type" prop is absent,
  so an empty plane property set is NOT enough; the minimal viable set is one primary plane bound to
  crtc0 with a real "type" property. Then re-test: anvil should build the DrmOutput, render, composite.
  Note: verify drm-rs get_property two-pass (count_enum_blobs) handling; plane_zpos/size_hints are
  ok().flatten()/optional. After planes, watch the plane-based ADDFB2/page-flip scanout (mostly wired
  for kmscube) for further gaps.

## ROOT CAUSE #6 FIXED (committed bb20f2c): plane+property model. anvil now advances past plane
   enumeration -> DrmCompositor -> "Initializing drm surface mode=Native 1280x800 @60" (mode timing
   perfect). New final wall below.

## ROOT CAUSE #7 (CURRENT WALL, landmine #1 "expensive option": PRIME/dmabuf) — HAND-OFF POINT
- smithay's DrmCompositor GbmFramebufferExporter allocates a GBM scanout buffer and EXPORTS it as a
  dmabuf (gbm_bo_get_fd -> DRM_IOCTL_PRIME_HANDLE_TO_FD) to build the DRM framebuffer for scanout.
  Kernel returns Unsupported -> "Failed to export the allocated buffer as dmabuf: Buffer returned
  invalid file descriptor" -> initialize_output fails -> anvil warns + returns (no scanout surface).
  Both explicit- and implicit-modifier attempts fail identically. This is exactly landmine #1.
- Also non-fatal WARNs (tolerated): "failed to create signaled syncobj" (EPERM; DRM_IOCTL_SYNCOBJ_*
  unimplemented) and "Preferred format AB30/AR30/AB24 not available" (plane advertises only XR24/AR24).
- FIX NEEDED (substantial kernel dmabuf subsystem; blocks M5 identically):
  real PRIME_HANDLE_TO_FD returning a dmabuf/VMO fd backed by the dumb buffer's physical pages, the
  ADDFB2-from-dmabuf mapping back to that buffer, and likely EGL dmabuf import of the compositor's GLES
  render target (EGL_EXT_image_dma_buf_import, already advertised). Kernel has shared memfd VMOs to
  build on. Watch for the plane-based page-flip + softpipe frame render after PRIME lands.
  Cheaper alternatives to probe first: a smithay/anvil framebuffer-exporter that uses gbm_bo_get_handle
  + ADDFB (kmscube's handle path, no dmabuf) instead of GbmFramebufferExporter — but anvil hardcodes
  GbmFramebufferExporter, so this likely needs an anvil/smithay patch, not just a flag.

## STATUS: anvil driven through 6 blockers (all fixed/committed) to the final PRIME/dmabuf scanout
   requirement. Full render stack (EGL/GLES softpipe, legacy DRM surface, wayland socket, XKB, input)
   all initialize. Exit (client composited + cursor + keys) is BLOCKED on #7. Reporting to orchestrator.

## NEXT LADDER (blocked on #7 = PRIME/dmabuf)
- [ ] Implement PRIME/dmabuf (see #7); re-test anvil composites.
- [ ] wl_shm xdg client composites (screenshot).
- [ ] QMP tablet motion moves cursor (2 screenshots); QMP keys reach client (color change/log).
- [ ] Both arches; regression (vfstest first, fresh images); kmscube still animates.
- [ ] Commit kernel DRM change; update wayland_cosmic_plan.md M4; M5 note: cosmic-comp needs same
      anvil-style is_software patch + likely the DRM-enum + these same kernel KMS ioctls.
