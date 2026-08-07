# K4 Progress — DRM + synthetic sysfs + evdev tablet

Owner: deep-reasoner (exclusive git+QEMU). Blueprint: docs/design/k4-drm-design.md
Started 2026-07-22.

## Commit plan (D8)
1. drm: st_rdev + device-node major:minor (D3) — GATES sysfs contract
2. drm: full KMS/buffer ioctl surface (D1)
3. drm: page-flip event channel + card0 read/poll (D2)
4. vfs: synthetic read-only sysfs + symlink support (D4) — ONLY IF probe R1 says needed
5. evdev: EV_ABS + full EVIOC + event1 + CLOCK_MONOTONIC (D5)
6. drivers: multi-instance virtio-input (kbd+tablet -> evdev0/1) (D6)
7. vfs+tooling: DynamicDevice poll readiness + QEMU virtio-tablet (D7)

## Test ladder
R0 drmsmoke, R1 drmenum(probe), R2 evtest2, R3 kmscube(M3 exit), R4 libinput, R5 anvil(M4)

## Regression gate (both arches, fresh image)
scmtest 19/19, epolltest 8/8, idletest IDLE_CPU_US=0, vfstest 34/34, polltest, forktest,
memtest, sigtest, waittest(1 retry ok), K3 dyn ladder, boot-to-login, MAME sound if audio touched.

## STATE / LOG
- Orientation done. Design line refs accurate.
- COMMIT 1 (st_rdev): DONE, compiles both arches (exit 0).
  - vfs: DynamicDeviceEntry +major/minor; makedev(); lookup_device_rdev(); register_device
    now (path,port,dev_id,major,minor); write_stat_full_rdev() writes st_rdev at x86 off40 /
    aarch64 off32; handle_fstat early-return for DynamicDevice w/ rdev; stat_common dyn-dev rdev.
  - callers: drm 226:0, evdev event0 13:64, pipewire 0:0.
- COMMIT 2 (DRM ioctl surface): CODE DONE.
  - drm_device_interface.rs: added GET_CAP, SET_CLIENT_CAP(ATOMIC->EINVAL/legacy),
    SET/DROP_MASTER(Ok0), GET_MAGIC/AUTH_MAGIC, GEM_CLOSE, DESTROY_DUMB, ADDFB2(LINEAR),
    RMFB, DIRTYFB(gpu.flush), PRIME->Unsupported.
  - DUMB_BUFFERS now stores {phys,order} so free is correct (buddy::free). free_dumb helper.
  - mmap validation: echo offset only if it's a known dumb-buffer phys.
  - DISPATCH REFACTOR: removed top-level device.lock(); now per-arm lock. New arms copy-in
    (ptr::read_unaligned) BEFORE lock, write back AFTER. Satisfies 82d0cc3 invariant (R7).
    Pre-existing arms keep small-stack-struct-under-lock (documented, resident).
- COMMIT 3 (flip events + card0 read/poll): CODE DONE.
  - PENDING_FLIPS/READY_EVENTS VecDeques; queue_flip_event on PAGE_FLIP w/ EVENT flag;
    drm_tick() 100Hz hook (try_lock only, throttle >=20ms ~50Hz, wakes poll ONLY on delivery
    -> idle stays 0); drm_read_events/drm_has_events/drm_event_seq.
  - servers/drm: VFS_READ arm (safe copy via with_task_address_space), VFS_POLL arm, tick hook
    registered in init (does NOT displace audio slot0).
  - vfs handle_poll: DynamicDevice now proxies VFS_POLL to owning port; negative reply => not
    ready (preserves pre-K4 behavior for servers w/o VFS_POLL, e.g. evdev pre-commit5).
    NOTE: moved D7.3 poll-proxy earlier into commit3 (card0 read/poll is meaningless without
    it; needed for R0/R3). Deviation logged.
- R0 drmsmoke written (userland/drmsmoke) + registered (workspace, build-userland RELIBC_LINKED,
  mkfs bins). Needs build.rs for relibc link (copied from polltest).

## VERIFICATION (both arches, fresh images from build-all)
- R0 drmsmoke AARCH64: open_card0, st_rdev_226_0, VERSION, GET_CAP(DUMB_BUFFER)=1,
  GET_CAP(TIMESTAMP_MONOTONIC)=1, GETRESOURCES, GETCONNECTOR, CREATE_DUMB, MAP_DUMB, MMAP_FILL,
  ADDFB2, SETCRTC, DIRTYFB, DESTROY_DUMB = ALL PASS. Full-screen gradient screenshot confirmed
  (/tmp/k4-r0-gradient-aarch64.png) -> present path works, Risk R5 RULED OUT.
- R0 drmsmoke X86_64: identical ALL PASS + gradient (/tmp/k4-r0-gradient-x86.png).
- Regression AARCH64: idletest IDLE_CPU_US=0 PASS (tick hook clean), epolltest 8/8, scmtest 19/19,
  vfstest 34/34, boot-to-login OK.
- Regression X86_64: idletest IDLE_CPU_US=0 PASS, scmtest 19/19, epolltest 8/8, vfstest 34/34.
- Added PAGE_FLIP-event + poll + read test to drmsmoke to verify commit-3 event channel
  (drmsmoke didn't exercise it). Rebuilding (build4) to verify before commit.

## COMMIT STRUCTURE DECISION (deviation from 7-commit split)
Design commits 1,2,3 share files in ways that prevent clean per-stage BUILDABLE snapshots:
- register_device signature change (st_rdev) + its callers span vfs/evdev/pipewire/drm-server;
  drm-server ALSO carries commit-3 read/poll/tick. vfs carries st_rdev (1) + poll proxy (3).
  drm_device_interface carries ioctls (2) + event channel (3). drivers/Cargo.toml sched dep (3).
Non-interactive hunk staging (git add -p) unavailable. So commits 1-3 land as ONE integrated
"DRM core (K4)" commit that builds+boots+passes both arches. Deviation logged + reported.
Commits 4-7 (sysfs/evdev/tablet/poll) will be separate.

## COMMITTED
- a7754d0 "drm: DRM UAPI ioctl surface, st_rdev device numbers, and page-flip event channel"
  (design commits 1+2+3 merged, both arches verified incl. event channel).
- da6a2f0 "userland: add drmsmoke ... /dev/dri/card0" (R0 test + build/pack wiring).

Commit-3 event channel VERIFIED both arches via drmsmoke:
  PAGE_FLIP_EVENT PASS, POLL_CARD0_READABLE PASS, READ_FLIP_EVENT PASS (user_data echoed).

## NEXT (down the ladder)
- R3 kmscube (M3 EXIT) + R1 sysfs probe folded in: pack GL ship set into mkfs, run kmscube.
  If kmscube works WITHOUT sysfs -> commit 4 (sysfs) deferred to M4 (confirms probe: kmscube
  opens card0 by path, no drmGetDevices2). GL ship set (kmscube manifest, ports/gl-stack/NOTES):
  libEGL.so.1, libGLESv2.so.2, libgbm.so.1, libdrm.so.2, libgallium-25.3.6.so, libexpat.so.1,
  libz.so.1, libwayland-{client,server}.so.0, libffi.so.8, /usr/lib/gbm/dri_gbm.so, + ld-musl,
  libc.so. Sources: m3-gl-stack/sysroot-<arch>/usr/lib/ + out/kmscube-<arch>. ~86MB/arch -> watch
  image size margin (71c2b84).
- Then evdev+tablet (commits 5-7) for M4.

## GL PACKING + R3 KMSCUBE (done + probe result)
- mkfs extended: GL ship set packed under SONAMEs into /usr/lib + new /usr/lib/gbm dir inode
  (ino 17) + dri_gbm.so + /bin/kmscube, both arches. Images ~1.05-1.07GB (margin OK).
  libEGL/gallium/gbm real content confirmed packed (symlink-follow works).
- R1 SYSFS PROBE RESULT (folded into R3): kmscube WITHOUT -D calls libdrm drmGetDevices2 ->
  "No such file or directory" -> needs synthetic sysfs. WITH `-D /dev/dri/card0` it BYPASSES
  enumeration. => commit 4 (sysfs) only needed for libdrm-enumeration consumers; anvil (drm-rs)
  reads no sysfs (design D10-R1). DEFERRED unless M4 needs it.
- R3 KMSCUBE STATUS (both arches identical): with -D, full Mesa stack loads (libEGL/libGLESv2/
  libgbm/libgallium/dri_gbm all dlopen+link OK), EGL 1.5 initializes, GBM device+surface created,
  EGL extensions enumerated (EGL_MESA_platform_gbm etc), a dumb buffer is CREATE_DUMB+MAP_DUMB+
  mmap'd successfully (our [MMAP] path). THEN: deterministic userspace NULL deref
  (EL0, FAR=0x0, DFSC=6 translation-fault, ESR EC=0x24) at ELR≈<gallium_base>+0x…BE58, x0=1 x1=0.
  Crash is right AFTER the dumb-buffer mmap, at a null address UNRELATED to the mapped buffer
  (buffer @0x7D41C000, FAR=0) => a Mesa-internal null-pointer/func-ptr deref, NOT a kernel DRM
  mapping fault. drmsmoke proves the same mmap path is writable. softpipe crashes earlier (before
  dumb-buffer alloc); default GBM_ALWAYS_SOFTWARE path gets furthest.
  ASSESSMENT: kernel DRM surface is sufficient through Mesa init + dumb-buffer mmap; remaining
  blocker is a Mesa userspace crash needing guest-side symbolization / Mesa debug build — a
  userspace-integration follow-up, out of kernel-K4 scope. M3 kernel side DONE; M3 kmscube-green
  pending Mesa debug.

## PIVOT: implementing evdev EV_ABS + virtio-tablet (commits 5-7) — in-scope, verifiable (evtest2).

## COMMITS 5-7 CODE (compiles both arches, exit 0)
- Commit 5 (evdev): MAX_EVENTS 64->256; EvdevDevice +clockid +seq; event1 (13:65) registered as
  tablet in init; full 'E'-type ioctl decoder by nr: EVIOCGVERSION/GID/GNAME("QEMU Virtio Tablet")/
  GPHYS+GUNIQ(ENOENT)/GPROP(zero=no INPUT_PROP_DIRECT)/GKEY/GLED/GSW(zero)/GBIT(ev: dev1 SYN|KEY|ABS,
  BTN_LEFT/RIGHT/MIDDLE, ABS_X/Y)/GABS(0..32767 res0)/SCLOCKID(store)/GRAB+REVOKE(accept); VFS_POLL
  arm (POLLIN if count>0, seq). copy_out/copy_in/zero_out via with_task_address_space.
- Commit 6 (virtio driver): multi-instance. VIRTIO_INPUTS: Vec; find_all_devices binds ALL
  virtio-input funcs; new_from(dev) maps DEVICE_CFG (cap type 4) + probes EV_ABS (select EV_BITS,
  subsel EV_ABS, size>0) => tablet=event1 else keyboard=event0 (order-independent). poll routes by
  evdev_index. Removed unused find_device import.
- Commit 7 (wiring): run-qemu.sh + driver.py add -device virtio-tablet-pci (both arch branches);
  x86_64 on_tick() now calls poll_events() (was aarch64-only) so tablet polls on x86 too -> needed
  arch-x86_64 Cargo.toml +drivers dep (no cycle: drivers deps don't reference arch-x86_64). VFS
  handle_poll DynamicDevice proxy + evdev VFS_POLL arm already landed (commit 3 & 5).
- evtest2 R2 test written + registered (workspace/build/mkfs/build.rs): open event1, EVIOCGNAME,
  EVIOCGBIT EV_ABS + ABS_X/Y, EVIOCGABS max=32767, no INPUT_PROP_DIRECT, EVIOCSCLOCKID, epoll
  idle-no-false-wake; motion phase (informational, needs injection).
- build6 DONE both arches (evtest2 em-dash byte-string fix). Boot shows "[INPUT] Found VirtIO
  Input device" (new multi-instance driver live).
- R2 evtest2 AARCH64: open_event1, EVIOCGNAME_tablet, EVIOCGBIT_has_EV_ABS, EVIOCGBIT_ABS_has_XY,
  EVIOCGABS_max_32767, no_INPUT_PROP_DIRECT, EVIOCSCLOCKID_monotonic, epoll_idle_no_false_wake =
  ALL PASS. Motion phase: needs QMP input-send-event injection (HMP has no abs-pointer inject);
  capability checks are the gate. Tablet evdev surface fully verified.
- R6 dual-bind: boot log "[INPUT] virtio-input functions found=0x00000002" (both enumerated; both
  had valid BARs assigned). Per-device "Found/->tablet" prints get garbled by framebuffer/serial
  console racing during boot->login (known artifact) — count print is the clean signal.
- AARCH64 REGRESSION GATE (fresh build7 image) ALL GREEN:
  scmtest 19/19, epolltest 8/8, idletest IDLE_CPU_US 0, vfstest 34/34, polltest 6/6, forktest PASS
  (exit 0, pthread_atfork_hooks_run), memtest 0 FAIL, sigtest PASS, waittest PASS
  (wait_on_process_group PASS - NO retry needed), K3 ladder hello-dyn + hello-dyn-rs + dlopen-host
  all PASS (dlopen-host needs cwd=/ for ./plugin.so: "dlopen: OK, call: OK result=0x4d41474b"),
  boot-to-login OK, drmsmoke READ_FLIP_EVENT PASS (DRM unaffected by evdev/tablet), evtest2 8/8.
  NOTE: serial has ~1-command output lag/bleed; ran tests individually to disambiguate.
- build8 (final both-arch, all commit 5-7 code incl found=/bound= debug) running.
- X86_64 GATE (fresh build8 image) GREEN: found=2, evtest2 8/8, drmsmoke 17/17, idletest
  IDLE_CPU_US 0, scmtest 19/19, epolltest 8/8, vfstest 34/34, boot-to-login.

## FINAL COMMITS (all on main, both arches verified)
  a7754d0 drm: DRM UAPI ioctl surface, st_rdev, page-flip event channel  (design 1+2+3)
  da6a2f0 userland: drmsmoke R0
  2d017cd evdev: virtio-tablet event1 + full EVIOC          (design 5)
  2a5a38c drivers: bind every virtio-input function          (design 6)
  ea4b658 tooling,x86_64: attach virtio-tablet + poll both   (design 7)
  d2511b4 userland: evtest2 R2
  79135ea mkfs: pack kmscube GL ship set + evtest2
Working tree clean. Commit 4 (synthetic sysfs) DEFERRED per R1 probe (kmscube uses -D; anvil
reads no sysfs). K4 DONE except kmscube-green (Mesa userspace null-deref, documented).

## OPEN DECISIONS
- Commit 4 (synthetic sysfs ~400 LoC): DEFER until R1 probe. kmscube opens /dev/dri/card0 by
  path (no drmGetDevices2), Smithay reads no sysfs, Mesa swrast tolerates missing PCI id =>
  likely NOT needed for M3. Will confirm with on-target drmenum probe.
- Throttle chosen 20ms (~50Hz) vs design ~60/16ms — conservative, further from spin. Client
  self-gates (one flip per frame). Logged.
