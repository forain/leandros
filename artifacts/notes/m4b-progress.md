# M4b wave — Mesa/libgbm fix so gbm_bo_get_fd reaches PRIME (anvil scanout export)

Owner: deep-reasoner M4-completion wave. Exclusive git/QEMU/images. Resume from checkpoint.

## STATUS @ 19:45 — M4 core DONE + validated; client window is the last piece
- COMMITS (main): 8a2a271 (kernel ioctl sign-ext mask -> gbm_bo_get_fd export works),
  5120ff9 (kernel SETCRTC presents fb -> compositor frame 0 scans out). BOTH KERNEL-SIDE.
  *** THERE IS NO MESA PATCH — stock Mesa works; nothing under ports/mesa is needed or present. ***
- aarch64 PROVEN (screenshot): anvil takes over console -> lavender desktop + cursor sprite tracking
  QMP virtio-tablet (m4br-aarch64-D/E). = scanout + render + cursor-follows-tablet. 2 of 3 M4 exit.
- REMAINING: wl_shm client window + key-to-client. wlclient stalled looking like "connected to
  display" only — LIKELY a stderr BLOCK-BUFFERING artifact (redirected stderr buffers, so stage
  prints never flushed = looked stuck). FIXED wlclient.c: setvbuf(_IONBF) + per-stage prints
  (globals, roundtrip done, shm buffer, toplevel committed, configure, alive heartbeat). Rebuilt
  both arches (build-wlclient.sh), aarch64 image regenerated. Running m4b_client2.sh (280s present +
  150s/90s handshake waits + key). Note black desktop = anvil idling on initial frame 0 (no damage);
  a client buffer-commit is damage -> anvil re-renders + composites the window (like cursor motion
  did). If wl.log now reaches "toplevel committed, waiting for configure" but no configure -> anvil
  xdg sequencing; if "configured -> painted" but no window -> anvil wl_shm compositing/import.
- HARNESS: driver.py _serial_send fixed (prompt-sync + 2-space pad) — env exports arrive intact;
  backup driver.py.bak. `sleep` WORKS in guest (brush builtin; 9-min scripts wait correctly).
- TODO after client: x86_64 pass (kernel already built w/ both fixes; image regen w/ stock Mesa +
  instrumented wlclient), full regression (vfstest first, drmsmoke 20/20, scmtest, epolltest,
  evtest2, idletest, kmscube -D), plan-doc M4 update.

## *** BREAKTHROUGH (2026-07-22 ~18:32): anvil PRESENTS + console takeover CONFIRMED ***
- With FIX #1 (kernel ioctl sign-extension mask, committed 8a2a271) + FIX #2 (std_handle_set_crtc
  presents the fb via handle_flip_page's software-scale+gpu.flush, UNCOMMITTED), anvil takes over
  the display: m4bc-aarch64-A-desktop.png is PURE BLACK 1280x800, NO console text (was console text
  before). Black = anvil's empty desktop clear color. Presentation/scanout WORKS.
- STILL NO Mesa patch (stock Mesa). The two fixes are 100% kernel-side. (Coordinator keeps expecting
  a ports/mesa patch — there is correctly none.)
- Gaps in that run: wlclient launch MANGLED over serial ("ayland-1"/"hellosleep" = driver.py drops
  the head of a command when the shell isn't at a fresh prompt; also QMP `key hello` leaked into the
  console brush because anvil + the kernel console SHARE the virtio-keyboard). So no client/cursor
  yet — all 5 shots were the same black frame.
- RECOVERY IN FLIGHT (m4b_recov.sh): (a) set env with short cmds after a wake-newline; (b) prove
  VISIBLE rendering via QMP tablet motion (cursor sprite on black, no serial input needed) — 2 shots
  center/corner; (c) robust wlclient launch (wake+short) -> client composites. Screenshots m4br-*.

## *** aarch64 VISIBLE RENDERING + CURSOR PROVEN (screenshot-verified) ***
- m4br-aarch64-D-client.png / E-client-cur.png: anvil's LAVENDER desktop (clear color) + a rendered
  CURSOR SPRITE at two positions (bottom-right vs center) driven by QMP virtio-tablet motion.
  => scanout + per-frame rendering + cursor-follows-tablet ALL PROVEN on aarch64.
- COMMITS: 8a2a271 (ioctl sign-ext mask), 5120ff9 (SETCRTC presents fb). Both kernel-side, no Mesa.
- DRIVER FIX (harness, not repo): driver.py _serial_send waits for the "#" prompt + prepends 2
  spaces -> env exports arrive intact. Backup driver.py.bak. QMP `key` also reaches the console
  brush (anvil+console share virtio-kbd) — verify keys reach the CLIENT via wl.log, not the shell.
- REMAINING: wl_shm client WINDOW not visible yet. wlclient.c is a correct xdg-shell client;
  m4b_client.sh captures /tmp/wl.log stages (connected/toplevel mapped/"configured -> painted") to
  localize: stalled-before-configure (anvil not sending configure) vs painted-but-not-composited
  (shm import). wlclient.c + build-wlclient.sh are editable test artifacts.


## NEXT WALL (post-export) + FIX #2: SETCRTC does not present the framebuffer
- After the export fix, anvil reaches DrmCompositor init -> SETCRTC ("Setting new mode: Native"),
  connects wl_shm clients, but NO screenshot shows anvil's desktop (still kernel console).
- ROOT CAUSE: drivers/src/drm/device.rs set_crtc only updates CRTC/plane STATE; it does NOT
  software-scale the fb into virtio-gpu resource 1 + gpu.flush like handle_flip_page (PAGE_FLIP).
  So drmModeSetCrtc(fb) presents nothing. kmscube survives via continuous PAGE_FLIPs; anvil's
  smithay legacy surface does set_crtc(fb) then a follow-up page_flip(fb,EVENT) (legacy.rs:314),
  but frame 0 stays invisible until that flip lands, and anvil appears to stall in DrmCompositor
  init/first-commit under TCG softpipe (anvil.log frozen at "Creating new Output"/mode-set across
  120s-apart samples).
- FIX #2 (drm_device_interface.rs std_handle_set_crtc, UNCOMMITTED): after device.set_crtc, if
  fb_id!=0, mirror handle_flip_page (software-scale + gpu.flush) to present frame 0 — correct
  drmModeSetCrtc semantics. Rebuilt aarch64 kernel; patient composite run next.
- rdebug caveat: crate::pci::rdebug ([DRM-IF]/[DRM] markers) writes to a PCI debug port the
  driver.py serial socket does NOT capture; verify presentation by PIXELS changing, not [DRM] logs.

## ROOT-CAUSE ANALYSIS (host-side, source-level) — DONE
- K5 proved: anvil's gbm_bo_get_fd() returns -1 and ZERO DRM_IOCTL_PRIME_HANDLE_TO_FD reach the
  kernel. Kernel PRIME is correct (drmsmoke 20/20, commit 6ce43be).
- gbm_dri_bo_get_fd (gbm_dri.c:443) returns -1 iff bo->image==NULL OR dri2_query_image(FD) fails.
- A DRIimage bo ALWAYS gets a softpipe displaytarget: gbm_dri_bo_create always ORs in
  __DRI_IMAGE_USE_SHARE (gbm_dri.c:932) -> PIPE_BIND_SHARED -> softpipe_resource_create_front
  allocates spr->dt (sp_texture.c:173). softpipe_resource_get_handle (sp_texture.c:254) then
  routes WINSYS_HANDLE_TYPE_FD to the kms-dri winsys drmPrimeHandleToFD (kms_dri_sw_winsys.c:521).
  => a DRIimage bo's get_fd WOULD issue the PRIME ioctl. Since marker=0, the bo is NOT a DRIimage.
- Therefore bo->image==NULL == create_dumb() fallback was taken (gbm_dri.c:902-903).
  create_dumb requires (usage & GBM_BO_USE_WRITE) OR (!has_dmabuf_export).
  - has_dmabuf_export is TRUE post-M3 (kernel DRM_CAP_PRIME=IMPORT|EXPORT, a13db0a; proven because
    kmscube's gbm_surface back buffers ALSO route through gbm_dri_bo_create and flipped to DRIimage
    post-M3 — that was the M3 crash fix).
  - anvil's primary allocator is RENDERING|SCANOUT (udev.rs:846), no WRITE; gbm core
    with_modifiers passes USE_SCANOUT only (gbm.c:519). So WRITE "shouldn't" be set — a residual
    source/runtime mismatch the instrumentation print will settle (which of the two conditions).
- Net: the fix must make gbm_bo_get_fd() succeed for a create_dumb bo. The kernel already supports
  PRIME export of dumb handles (6ce43be), so exporting bo->handle via drmPrimeHandleToFD is correct
  and works regardless of WHY the dumb fallback was taken.

## FIX APPLIED (Mesa patch, port policy allows) — ports/mesa/0001-gbm-dri-prime-fallback-dumb-scanout.patch
- gbm_dri.c gbm_dri_bo_get_fd: when bo->image==NULL, drmPrimeHandleToFD(dri fd, bo->handle,
  DRM_CLOEXEC|DRM_RDWR) and return that fd. + added #include <fcntl.h> (O_CLOEXEC/O_RDWR).
- TEMP instrumentation (remove before final commit): fprintf in bo_create (prints branch DUMB/
  DRIIMAGE + usage + has_export + count) and in bo_get_fd (image NULL, PRIME ok/fail).
- Source edited in VOLATILE ~/.claude-forain/jobs/afde2e74/tmp/mesa-wave2/src/mesa (orig kept as
  gbm_dri.c.orig). Patch saved durably under ports/mesa/.

## BUILD — incremental works and is FAST (~30s)
- /tmp/m4b_build_gbm.sh <arch> : ninja -C build/mesa-wayland-<arch> dri_gbm.so (+libgbm). Needs
  PATH=brew bison, PYTHONPATH=s3-mesa-probe/.venv site-packages, zig/ninja on PATH.
- aarch64 dri_gbm.so REBUILT rc=0, markers present, DEPLOYED to
  sysroot-aarch64/usr/lib/gbm/dri_gbm.so (sha 0742272...). x86_64 NOT yet built.

## NEXT (resume here)
1. Regenerate aarch64 f2fs image (scripts/mkfs-f2fs-populated.py packs from m3-gl-stack sysroots +
   m4-input-ship); verify on-image dri_gbm.so checksum == new.
2. Boot aarch64 uefi-tcg (NOT HVF — virtio-input hang), launch anvil:
   ANVIL_DRM_DEVICE=/dev/dri/card0 SMITHAY_USE_LEGACY=1 XDG_RUNTIME_DIR=/run/user/0 anvil --tty-udev
   Capture serial (m4_capture-style persistent socket). Look for [GBM] lines: confirm DUMB branch +
   whether PRIME fallback OK. Then wlclient composites; QMP tablet/keys.
3. If export now succeeds but RENDER path fails (EGL dmabuf import to bind buffer as GL target):
   next wall is dri2_from_dma_bufs -> softpipe displaytarget_from_handle (kms_dri_sw_winsys.c:473,
   PRIME_FD_TO_HANDLE) — kernel supports it; watch for modifier rejection.
4. Build x86_64 dri_gbm.so, deploy, repeat.
5. Remove instrumentation; clean rebuild both; fresh images; full regression (vfstest FIRST,
   drmsmoke 20/20, scmtest, epolltest, evtest2, idletest; kmscube -D animates).
6. Commit (patch under ports/mesa + repo), update wayland_cosmic_plan.md M4, screenshots to
   notes/m4-screenshots/.

## *** CORRECTED ROOT CAUSE (from instrumented run — overturns the create_dumb theory) ***
Instrumented aarch64 anvil run captured (guest has NO sh/grep — use brush `while read;case`):
  [GBM] bo_create 1280x800 fmt=AR24 usage=0x1 has_export=1 count=1 -> DRIIMAGE
  [GBM] bo_create 1280x800 fmt=AR24 usage=0x5 has_export=1 count=0 -> DRIIMAGE
  [GBM] bo_create 1280x800 fmt=AR24 usage=0x1 has_export=1 count=1 -> DRIIMAGE
  ... Initializing drm surface (mode Native 1280x800@60) ...
  WARN anvil::udev: Failed to initialize drm output: Failed to export the allocated buffer as
       dmabuf: Buffer returned invalid file descriptor
- Branch is DRIIMAGE (has_export=1), NOT create_dumb. My first fix (bo->image==NULL fallback) was
  DEAD CODE — never triggered (no [GBM] get_fd line printed).
- Real failure: for the count=0 scanout bo (usage=0x5 SCANOUT|RENDERING), dri_create_image ->
  softpipe resource_create -> displaytarget (bind has SCANOUT|SHARED). bo->image != NULL.
  gbm_dri_bo_get_fd calls dri2_query_image(bo->image, ATTRIB_FD) which returns FALSE ->
  gbm_bo_get_fd returns -1. (count=1 calls return NULL bo: softpipe has no
  resource_create_with_modifiers, dri_create_image bails at count>0 — smithay falls back to the
  no-modifier create_buffer_object, which is the count=0 success.)
- WHY ATTRIB_FD fails is the open question the SECOND instrumented build answers. Source path:
  dri2_query_image FD -> common(no) -> by_resource_param(softpipe has NO resource_get_param -> no)
  -> by_resource_handle -> softpipe_resource_get_handle (unconditionally calls winsys
  displaytarget_get_handle for FD -> drmPrimeHandleToFD). So it SHOULD reach the kernel PRIME.
  Either the kernel PRIME fails for this handle, or something short-circuits. Kernel PRIME handler
  (syscall.rs:5602) ignores flags, looks up handle in DUMB_BUFFERS via dumb_buffer_phys_order,
  returns EINVAL(-22) if not found. The winsys dumb buffer IS made via CREATE_DUMB (kms winsys:196)
  so the handle should be registered — unless a per-fd/registry mismatch.

## FIX v2 (rebuilt+deployed aarch64, re-probing): gbm_dri_bo_get_fd now, when ATTRIB_FD fails,
  queries ATTRIB_HANDLE (KMS dumb handle — the path kmscube uses successfully) and calls
  drmPrimeHandleToFD on it directly, for BOTH DRIimage and dumb bos. Full instrumentation prints
  image ptr, kms_handle, and PRIME result+errno. If the kernel PRIME succeeds on the queried
  handle, export is FIXED. If it fails EINVAL, the handle isn't in DUMB_BUFFERS = kernel-side gap.

## *** TRUE ROOT CAUSE (2nd instrumented run) — KERNEL sign-extension bug, NOT Mesa ***
Instrumented get_fd (v2) captured on aarch64:
  [GBM] get_fd: image=0x5068ddd0 kms_handle=1 -> PRIME export
  [GBM] get_fd: PRIME export FAILED errno=1   (EPERM)
- bo IS a DRIimage; ATTRIB_HANDLE gives a valid dumb handle (1); drmPrimeHandleToFD(1) -> EPERM.
- EPERM(-1) == drm_device_interface.rs:520 `PRIME_* => Err(Unsupported)` = the DRM SERVER's reply.
  So the kernel PRIME intercept (syscall.rs:5602 `if cmd == 0xC00C642D`) was BYPASSED and the ioctl
  fell through to the is_drm VFS forward -> DRM server -> EPERM. (An intercept HIT would give
  EINVAL(-22) for an unknown handle or success — never EPERM.)
- WHY bypassed: musl's `int ioctl(int fd, int request, ...)` SIGN-EXTENDS the request into the raw
  syscall. 0xC00C642D has bit 31 set, so anvil/libdrm/Mesa (all musl) send the kernel
  cmd=0xFFFFFFFF_C00C642D. `cmd == 0xC00C642D` fails; ioctl_type=(cmd>>8)&0xFF is still 0x64 so
  is_drm forwarding still works for every OTHER DRM ioctl — only the exact-match PRIME intercept
  breaks. drmsmoke uses Rust libc `ioctl(fd, request: c_ulong, ...)` = ZERO-extended = matches the
  intercept = why drmsmoke PRIME passed 20/20 while every musl client (anvil, and cosmic-comp next)
  fails. This ALSO explains K5's "marker=0": the marker lived inside the never-reached intercept.
- => K5's conclusion "kernel PRIME done, wall is Mesa userspace" was a misread. The kernel PRIME
  IMPL is correct but its DISPATCH has a sign-extension bug. This is a KERNEL fix, no Mesa patch
  strictly required.

## FIX v3 (THE fix): kernel syscall.rs sys_ioctl — mask cmd to 32 bits at entry
  `let cmd = cmd & 0xFFFF_FFFF;` normalizes musl's sign-extended ioctl request so the PRIME
  intercept (and any exact-match dispatch) matches. Fixes the whole class for all musl clients
  (anvil + cosmic-comp). Building aarch64 kernel+image now.
  - With this, the STOCK Mesa path (dri2_query_image ATTRIB_FD -> softpipe -> winsys
    drmPrimeHandleToFD -> intercept now matches) should work with NO Mesa patch. Plan: confirm with
    the currently-deployed instrumented Mesa (it tries ATTRIB_FD first -> expect "ATTRIB_FD OK"),
    THEN REVERT the Mesa gbm_dri.c patch to stock (rebuild dri_gbm both arches) so we ship no Mesa
    change. Keep ports/mesa/0001-*.patch only if stock somehow still needs the HANDLE fallback.

## MUSL EVIDENCE (airtight): musl-dynamic/src/musl-1.2.5/src/misc/ioctl.c:128
  `int ioctl(int fd, int req, ...)` -> line 135 `__syscall(SYS_ioctl, fd, req, arg)` passes the
  `int` req to a long-arg syscall = SIGN-EXTENSION. sys/ioctl.h:115 declares `int ioctl(int,int,...)`.
  LeandrOS userland (drmsmoke, etc.) links relibc (build-all.sh "Building aarch64 relibc"), whose
  ioctl request is c_ulong/usize = zero-extended = why drmsmoke's PRIME matched the intercept.

## *** FIX CONFIRMED (aarch64, post kernel rebuild) ***
Probe after `cmd & 0xFFFF_FFFF` kernel fix:
  [GBM] get_fd: DRIimage ATTRIB_FD OK fd=16
  -> the STOCK dri2_query_image(ATTRIB_FD) -> winsys drmPrimeHandleToFD path now matches the
     intercept and returns a real dmabuf fd. The "Failed to export ... invalid file descriptor"
     WARN is GONE. anvil advanced past the export wall. Proves NO Mesa patch is needed.
  (probe2 screenshot still showed console at 55s — need a longer run to see first scanout; running
   m4b_exit2.sh: 45s wait + wlclient + QMP.)

## FINALIZATION PLAN (once exit2 confirms composite+cursor+keys)
1. REVERT Mesa gbm_dri.c to gbm_dri.c.orig (fully stock — no patch, no prints). Rebuild dri_gbm
   both arches (/tmp/m4b_build_gbm.sh), redeploy to sysroot-*/usr/lib/gbm/, DELETE
   ports/mesa/0001-*.patch (kernel fix is the sole change). Regenerate BOTH data images.
2. x86_64 kernel already built with the fix; run x86_64 exit test.
3. Full regression BOTH arches, FRESH images: vfstest FIRST, drmsmoke 20/20, scmtest, epolltest,
   evtest2, idletest; kmscube -D animates.
4. Commit kernel/src/syscall.rs (the mask). Update wayland_cosmic_plan.md M4. Final checkpoint.
Minor kernel gap noted (non-blocking): anvil triggers `faccessat2` (syscall 439/0x1B7) -> ENOSYS
   once at startup; anvil continues fine (libc falls back). Could add faccessat2 later.

## MESA REVERTED TO STOCK (no patch needed). gbm_dri.c restored from .orig, rebuilt both arches
  (0 [GBM] markers), deployed stock dri_gbm.so to both sysroots. The ONLY code change is the kernel
  cmd-mask in syscall.rs. ports/mesa/0001-*.patch to be DELETED at finalize (kept transiently).
  Images to regenerate with stock dri_gbm after QEMU stops.

## OPEN: does anvil fully SCAN OUT to the display? Export is fixed (fd=16) + anvil renders (mmap
  0xBBC00000), but screenshots at 45s/60s still show the console, not anvil's desktop. anvil.log
  tail is swamped by the ~2400-byte GL-extensions line. Running exit4 (70s wait, tail -c 9000,
  ps process check, proper WAYLAND_DISPLAY client) to determine: TCG-softpipe slowness vs a next
  wall (SETCRTC/page-flip/vblank). kmscube DOES scan out dumb buffers via SETCRTC+PAGE_FLIP, so the
  kernel scanout path works for that shape — anvil uses the same ADDFB2(handle)+SETCRTC.

## COMMITTED: 8a2a271 "kernel: mask sign-extended ioctl request so musl clients hit the PRIME
   intercept" (kernel/src/syscall.rs only). ports/mesa patch deleted (stock Mesa works). Both data
   images regenerated with STOCK dri_gbm + fixed kernel. Kernel images both arches built with fix.

## exit4 evidence (aarch64, stock-equivalent): `[wlclient] connected to display` — anvil's wayland
   compositor accepts wl_shm client connections + services roundtrip. anvil.log guest-timestamps
   frozen at 00:00:03.68 at ~2.5min WALL = anvil busy in softpipe first-frame render (CPU-bound
   under TCG), NOT stalled/erroring (no panic/WARN after export). => the visual composite is gated
   by TCG softpipe SPEED, not a correctness wall.

## PATIENT run in flight (aarch64): anvil bg + 300s wait + wc -l/tail (past GL line) + client +
   180s + QMP. Determines whether anvil scans out its desktop given enough wall-clock. If wc -l
   grows past Output-creation -> anvil progressed (render/scanout). If stuck -> a real post-export
   wall (SETCRTC not updating virtio-gpu display / vblank) to chase next.

## PROGRESS LOG
- aarch64+x86_64 dri_gbm.so built (incremental, patched src) + deployed to sysroots. aarch64 image
  regenerated; on-image dri_gbm.so = 591120 bytes (new). 
- aarch64 anvil diag (uefi-tcg, 30s): anvil boots full stack, EGL/GLES2 softpipe OK, "Trying to
  setup connector HDMI-A-1", "Creating new Output HDMI-A-1". Guest has NO `grep` (multicall
  coreutils) — extraction via shell-only `while read; case` filter instead. Giant GL-extensions
  line eats plain `tail`. Note [SYSCALL] ENOSYS nr=0x1B7 (439) fired once (some libc probe; anvil
  continued).
- Running /tmp/m4b_exit.sh aarch64: anvil BACKGROUNDED + shell-grep [GBM]/export markers +
  screenshots + wlclient + QMP. Awaiting result = the decisive export-fix verdict.

## M5 ANSWER — cosmic-comp TOLERATES software EGL (definitive; ../cosmic-epoch/cosmic-comp)
- device.rs init_egl() (line 158): creates EGLDisplay/EGLDevice/EGLContext with NO is_software
  check — software EGL passes. is_software is read AFTER (line 732), proving init succeeds.
- device_added (line 760-812): for is_software it SKIPS the hw-accel per-render-node client socket
  (create_socket) + DrmLeaseState (warns, sets None, NO panic/bail), but STILL builds the scanout
  GbmDrmOutputManager (line 797) with GbmAllocator RENDERING|SCANOUT (line 799-801) — IDENTICAL to
  anvil. => cosmic-comp needs NO is_software patch (unlike anvil), and hits the EXACT SAME
  gbm_bo_get_fd export path -> my Mesa fix is REQUIRED and SUFFICIENT for cosmic-comp scanout too.
- Bonus M5 knob: COSMIC_DRM_ALLOW_DEVICES env allowlist (device.rs:201) = cosmic's device-selection
  analog to anvil's ANVIL_DRM_DEVICE, useful given our synthetic /sys enumeration.
- M5 residual risks (same as anvil, NOT the software-EGL question): DRM udev enumeration on
  synthetic /sys, the KMS ioctls (planes/props), and whether clients get hw-accel (they won't on
  software -> wl_shm path only, which is fine).
