# K4 — DRM + synthetic sysfs + evdev tablet: Implementation Design

STATUS: COMPLETE 2026-07-22. All research done; full design in §D below. The §K/§C reference
sections that follow this header are the evidence base; §D is the executable plan.

## Scope (from plan §3 Wave K4 / §5 M3+M4)
- (a) DRM ioctl surface for Mesa kms_swrast + GBM dumb-buffer legacy KMS.
- (b) minimal read-only synthetic sysfs (/sys/dev/char, /sys/class/drm, /sys/class/input).
- (c) evdev: surface virtio-tablet as evdev absolute-pointer device with ABS_* semantics; extend EVIOC*.

## VFS/syscall plumbing findings (agent 4, servers/vfs/src/lib.rs + kernel/src/syscall.rs)
- Synthetic files = flat `RamEntry{path,data}` table (vfs:1074/1113); dirs = separate `RAMFS_DIRS` allow-list (:1138, holds b"/proc"). Matched by linear path_eq. Stat = S_IFREG|0644 size=data.len.
- **NO symlink support outside tmpfs.** handle_readlink (:4819) returns -EINVAL for RAMFS/RAMFS_DIRS paths. /proc/self/{exe,fd/N,maps} are hardcoded in kernel sys_readlinkat (syscall.rs:4931-4966), not VFS. sysfs symlinks (subsystem→bus, /sys/dev/char/226:0→device) have NO mechanism — MUST BUILD.
- getdents64 (:3687) synthesizes intermediate dirs (already splits /dev/dri/card0 → emits `dri` DT_DIR then `card0`), so /sys/class/drm listing card0 is achievable IF dirs added to RAMFS_DIRS.
- Dynamic per-open content EXISTS: gen_proc_system (:2063) renders bytes at open into an ephemeral tmpfs slot (e.g. /proc/mounts from live registry). uevent files fit this exactly → add gen_sys_content.
- **RECOMMENDATION: extend in-VFS RAMFS/RAMFS_DIRS + gen_sys_content + new symlink table, NOT a mount server.** servers/proc is the PROCESS-TABLE server, not a /proc FS — no pattern to mirror.
- Device mmap chain WORKS: sys_mmap (syscall.rs:1375) → DynamicDevice → VFS_IOCTL cmd 0x1007 offset=arg → DRM handle_ioctl_mmap (drm_device_interface.rs:830) → returns phys → map_device (mm/src/vmm.rs:269). Today MAP_DUMB returns phys as the mmap offset and handle_ioctl_mmap echoes it, so **standard CREATE_DUMB→MAP_DUMB→mmap already works** (cookie==phys). Gaps: no validation that offset ∈ allocated buffers; mapping is CACHED (prot_to_page_flags syscall.rs:1355 sets no WC/PAT). WC is perf-only for softpipe → DEFER.
- IOCTL dispatch (vfs:4226): synchronous, in caller's AS; forwards dev_id@data[0], cmd@[8], arg@[16], **pid@[24]**. Confirms DRM server's raw-arg-deref assumption. evdev uses with_task_address_space(pid); DRM ignores pid and relies on synchronous AS.
- **Poll/epoll GAP: handle_poll (vfs:4303) returns (0,0) for DynamicDevice (:4369) — NEVER ready.** Only fd0 console works via a kernel fast-path calling evdev_server::has_key_event(0) directly (syscall.rs:3413). epoll wait loop (syscall.rs:5978) correctly DROPS EPOLL_INSTANCES.lock before probe_fd_events_seq (:5988) per 82d0cc3 — new device poll must stay lock-free w.r.t. that lock. MUST add a DynamicDevice arm to handle_poll that proxies VFS_POLL to the owning port + generalize readiness off dev_id + device server calls try_wake_poll (evdev push_event already does :300; DRM must add).
- **st_rdev ALWAYS 0.** register_device(path,port,dev_id) (vfs:686) has no major/minor; write_stat_full (:5507) zeroes struct and never writes st_rdev; DynamicDevice fstat reports S_IFCHR|0666 ino0 (:5780). **libdrm computes major(st_rdev)=226 to build /sys/dev/char/226:0** → with rdev=0 it looks at /sys/dev/char/0:0. MUST plumb rdev (226:0 for card0, 13:64/65 for eventN) through the registry + write_stat_full.

## Mesa/libdrm ioctl split (agent 1) — CRITICAL
- **GBM/kms_swrast CLIENT issues ONLY buffer ioctls**: VERSION, CREATE_DUMB, MAP_DUMB, DESTROY_DUMB, GEM_CLOSE, PRIME_{HANDLE_TO_FD,FD_TO_HANDLE}. NO GET_CAP, NO KMS modeset. Backend selection uses VERSION.name only; NO hard match on "virtio_gpu" on swrast path; graceful fallback to swrast. GBM_ALWAYS_SOFTWARE=1 forces it.
- **COMPOSITOR (libdrm / drm-rs) issues KMS**: GET_CAP, GETRESOURCES, GETCONNECTOR(2-pass), GETENCODER, GETCRTC, ADDFB2(pref)/ADDFB(fallback is compositor policy, libdrm has none), SETCRTC, PAGE_FLIP+event read, DIRTYFB, RMFB, SET/DROP_MASTER, GET/AUTH_MAGIC(legacy, skipped w/ render nodes).
- kms_swrast winsys: src/gallium/winsys/sw/kms-dri/kms_dri_sw_winsys.c (CREATE_DUMB:196, MAP_DUMB+mmap:325, DESTROY_DUMB:219). GBM dri: src/gbm/backends/dri/gbm_dri.c create_dumb:828.
- **DIRTYFB often MANDATORY for virtio-gpu**: flushes dumb buffer to host after CPU render; without it (or a fresh SETCRTC/PAGE_FLIP triggering transfer) the display won't update. Tolerates -ENOSYS.
- Confirmed struct sizes (x86_64==aarch64, all 64-bit): drm_version 64 (only pointer-width struct), drm_mode_crtc 104, drm_mode_get_connector 80, drm_mode_card_res 64, drm_mode_get_encoder 20, drm_mode_fb_cmd2 104, drm_mode_fb_cmd 28, drm_mode_create_dumb 32, drm_mode_map_dumb 16, drm_get_cap 16, drm_gem_close 8, drm_auth 4. drm_event 8, drm_event_vblank 32 (DRM_EVENT_FLIP_COMPLETE=0x02).

## CORRECTIONS to task/plan assumptions (verified in source)
1. **virtio-tablet is NOT attached in QEMU today.** run-qemu.sh:192 and driver.py:149 attach only `virtio-keyboard-pci`. K4 MUST add `-device virtio-tablet-pci` to BOTH, and a virtio-input driver instance bound to it. (The plan's "virtio-tablet attached by default" is stale/aspirational.)
2. **The "dormant richer drm_driver.rs" is NOT the reusable asset for Mesa** — it uses a bespoke serialize() wire format with made-up cmd numbers, not Linux UAPI. The live UAPI handler is drm_device_interface.rs; K4 extends THAT, pulling object-model/auth from the drm/ submodule.
3. **libdrm needs a real bus in synthetic sysfs** (agent 2): subsystem symlink + device/drm dir + uevent with PCI_SLOT_NAME (virtio→pci) or platform MODALIAS, else drmGetDevices2 DROPS card0. Driver name comes from VERSION ioctl, not sysfs. GBM_ALWAYS_SOFTWARE=1 forces kms_swrast→swrast.
4. **virtio_keyboard.rs is really a generic virtio-input driver** hardcoded to evdev index 0. A tablet driver = a second instance bound to the tablet PCI device, pushing EV_ABS/EV_KEY/EV_SYN to a new evdev index.

## Kernel state findings (verified in source)

### DRM live node
- `/dev/dri/card0` registered by `servers/drm/src/lib.rs::init` → IPC port; VFS_IOCTL (tag 0x28) dispatched to `DrmDeviceInterface::handle_ioctl(cmd, arg)`. **The handler runs SYNCHRONOUSLY IN THE CALLER'S ADDRESS SPACE** (comment servers/drm/src/lib.rs:75) so `arg as *mut struct` raw derefs are valid. BUT derefs happen under `device.lock()` (spin::Mutex) — 82d0cc3 hazard shape; K4 new ioctls MUST copy-in/out outside the lock.
- Real UAPI handler = `drivers/src/drm_device_interface.rs`. Already wired: VERSION(0xC0406400), MODE_GETRESOURCES(0xC04064A0), GETCONNECTOR(0xC05064A7), GETENCODER(0xC01464A6), GETCRTC(0xC06864A1), CREATE_DUMB(0xC02064B2), MAP_DUMB(0xC01064B3), ADDFB(0xC01C64AE), SETCRTC(0xC06864A2), PAGE_FLIP(0xC01864B0), + custom 0x1001-0x1007 (DOOM path).
- GAPS for Mesa/GBM/Smithay-legacy (AUTHORITATIVE codes are in the "Authoritative DRM ioctl request codes" section and §D1 table — some early-draft values in THIS line were superseded; trust §D1): GET_CAP 0xC010640C, DESTROY_DUMB 0xC00464B4, GEM_CLOSE 0x40086409, ADDFB2 0xC06864B8, RMFB 0xC00464AF, SET_MASTER 0x641E, DROP_MASTER 0x641F, GET_MAGIC 0x80046402, AUTH_MAGIC 0x40046411, SET_CLIENT_CAP 0x4010640D, DIRTYFB 0xC01864B1; plus DRM event read()/PAGE_FLIP-event delivery (handle_read returns 0 — no drm_event_vblank).
- `drivers/src/drm_driver.rs` (the "dormant richer driver"): its `handle_drm_command` uses a BESPOKE serialize() wire format + made-up cmd nums (0x00/0x1e/...) that DO NOT match Linux UAPI. NOT directly reusable for Mesa. The reusable assets are the `drm/` submodule: `auth.rs` (create_session/set_master/drop_master/authenticate_session/can_perform), `device.rs` (DrmDevice object model), `framebuffer.rs`, `modes.rs`, `properties.rs`. K4 wires those INTO drm_device_interface.rs, not drm_driver.rs.

### evdev (`servers/evdev/src/lib.rs`)
- Only event0 (keyboard) registered (init:308-310). Uses safe pattern: `with_task_address_space(pid, ...)` for user writes + `arch_interrupt_save/restore` around `DEVICES.lock()`. Read path uses `write_user_buf`.
- input_event struct = timeval{i64,i64} + u16 type + u16 code + i32 value = 24 bytes (64-bit). Correct.
- Wired ioctls: FIONREAD(0x541B), EVIOCGVERSION(0x80044501→0x00010001), EVIOCGID(0x80084502), EVIOCGBIT(ev,len) only for ev==0 (returns 0x03 = EV_SYN|EV_KEY) and ev==1 (EV_KEY, writes 0xFF all keys). Everything else → ENOTTY/0.
- GAPS: no event1/event2 device; no EV_ABS/EV_REL in EVIOCGBIT(0); no EVIOCGABS; no EVIOCGNAME/PHYS/UNIQ/PROP; no EVIOCGKEY/LED/SW; no EVIOCSCLOCKID; no EVIOCGRAB. push_event already calls try_wake_poll() (K2 integration present).
- MAX_DEVICES=4, MAX_EVENTS=64 ring (tablet motion needs bigger ring — flag).

### Authoritative DRM ioctl request codes (computed from UAPI, type='d'=0x64; _IOC=(dir<<30)|(size<<16)|(type<<8)|nr; dir none=0/W=1/R=2/RW=3)
- SET_MASTER   = DRM_IO(0x1e)                       = 0x0000_641E  (size 0)  [gap]
- DROP_MASTER  = DRM_IO(0x1f)                       = 0x0000_641F  (size 0)  [gap]
- GET_MAGIC    = DRM_IOR(0x02, drm_auth{u32})       = 0x8004_6402  [gap]
- AUTH_MAGIC   = DRM_IOW(0x11, drm_auth{u32})       = 0x4004_6411  [gap]
- GEM_CLOSE    = DRM_IOW(0x09, drm_gem_close{u32 handle,u32 pad}=8) = 0x4008_6409  [gap]
- GET_CAP      = DRM_IOWR(0x0c, drm_get_cap{u64 cap,u64 val}=16)   = 0xC010_640C  [gap — only custom 0x1006 exists]
- SET_CLIENT_CAP = DRM_IOW(0x0d, drm_set_client_cap{u64,u64}=16)   = 0x4010_640D  [gap — UNIVERSAL_PLANES/ATOMIC toggles; ret 0]
- DESTROY_DUMB = DRM_IOWR(0xB4, {u32 handle}=4)     = 0xC004_64B4  [gap]
- ADDFB2       = DRM_IOWR(0xB8, drm_mode_fb_cmd2=104)= 0xC068_64B8  [gap; LINEAR only]  (size 104=0x68: 5u32+12u32+pad4+4u64)
- RMFB         = DRM_IOWR(0xAF, uint=4)             = 0xC004_64AF  [gap]
- GETPLANERES  = DRM_IOWR(0xB5, drm_mode_get_plane_res=16) = 0xC010_64B5  [optional]
- GETPLANE     = DRM_IOWR(0xB6, drm_mode_get_plane=32)     = 0xC020_64B6  [optional]
- WAIT_VBLANK  = DRM_IOWR(0x3a, union=~32)          = 0xC020_640A? [optional — swrast doesn't need]
- Already wired: VERSION 0xC0406400, GETRESOURCES 0xC04064A0, GETCRTC 0xC06864A1, SETCRTC 0xC06864A2, GETENCODER 0xC01464A6, GETCONNECTOR 0xC05064A7, ADDFB 0xC01C64AE, PAGE_FLIP 0xC01864B0, CREATE_DUMB 0xC02064B2, MAP_DUMB 0xC01064B3.
- DRM event read: struct drm_event{u32 type,u32 length}=8; drm_event_vblank{drm_event base; u64 user_data; u32 tv_sec,tv_usec,sequence,crtc_id}=32. DRM_EVENT_FLIP_COMPLETE=0x02, DRM_EVENT_VBLANK=0x01.

### virtio-gpu present path (drivers/src/virtio_gpu.rs)
- create_resource_2d, attach_backing, set_scanout(:575), transfer_to_host_3d(:636), flush(:591). SETCRTC/PAGE_FLIP already drive this via handle_create_framebuffer/std_handle_addfb (res_id = handle+10). KMS takeover disables fb console via framebuffer::set_console_disabled(true) (drm_device_interface.rs:242-245, triggered on SETCRTC/PAGE_FLIP).

### DRM master state (drivers/src/drm/auth.rs)
- AuthManager global (AUTH_MANAGER) with create_session/authenticate_session/set_master/drop_master/is_master/can_perform/close_session. Single-master model already present — reuse for SET/DROP_MASTER. NOTE: running-as-root single-seat means SET_MASTER can just succeed (return 0) without real magic auth; Mesa/Smithay call SET_MASTER once at open. can_perform gating currently requires authenticated+master for ModeSet — must NOT block the root compositor; simplest: SET_MASTER returns 0 and SETCRTC/PAGE_FLIP do not gate on master for bring-up.

## Consumer findings (agents)

### libinput 1.27.1 (agent 3) — evdev classification & ioctl surface
- **libinput classifies by udev STRING PROPERTY, not cap bits** (src/evdev.c:75-89,105,1689). `ID_INPUT=1` mandatory; `ID_INPUT_MOUSE=1` → absolute POINTER (POINTER_MOTION_ABSOLUTE) — exactly what Smithay wants for a QEMU tablet.
- **The "one-line flip" = a SHIM change** (userland lane), not kernel: libudev shim event2 must return `ID_INPUT_MOUSE=1` instead of `ID_INPUT_TOUCHSCREEN=1`. Kernel side is unaffected by the flip itself.
- **Kernel evdev MUST truthfully report** (via libevdev's ioctl battery): EV_ABS with BOTH ABS_X and ABS_Y (xor → reject), each with input_absinfo min!=max (0..32767), resolution 0 on both axes; EV_KEY with BTN_LEFT (+RIGHT/MIDDLE); NO INPUT_PROP_DIRECT, NO BTN_TOUCH, NO BTN_TOOL_*/MT axes.
- libinput→libevdev issues: EVIOCGVERSION(0x01), EVIOCGID(0x02), EVIOCGNAME(0x06,len), EVIOCGPHYS(0x07), EVIOCGUNIQ(0x08), EVIOCGPROP(0x09,len), EVIOCGBIT(0x20+ev,len), EVIOCGABS(0x40+abs → input_absinfo 24B), EVIOCGKEY(0x18), EVIOCGLED(0x19), EVIOCGSW(0x1b), **EVIOCSCLOCKID(0xa0, int)** — kernel must stamp input_event.time from CLOCK_MONOTONIC after this. type='E'=0x45. libinput never calls EVIOCGRAB but compositor may — accept gracefully.
- Every logical motion must end with EV_SYN/SYN_REPORT(0,0,0) or no motion is emitted (evdev-fallback.c:988).
- struct input_id{u16 bustype,vendor,product,version}=8B; input_absinfo{s32 value,min,max,fuzz,flat,resolution}=24B; input_event 24B (64-bit).

### Smithay legacy (non-atomic) DRM backend (agent 3, from-knowledge; no local checkout — cosmic-comp pins git rev efeb597)
- Uses drm-rs RAW ioctls on card0 fd; does NOT use libdrm C, does NOT read /sys for KMS. (Still needs libudev shim only to enumerate the node path.)
- Delta over Mesa render path = the whole KMS surface: SET/DROP_MASTER; GET_CAP (DUMB_BUFFER, CRTC_IN_VBLANK_EVENT, TIMESTAMP_MONOTONIC, PRIME, ADDFB2_MODIFIERS); SET_CLIENT_CAP(UNIVERSAL_PLANES; does NOT set ATOMIC — ignoring ATOMIC selects legacy path); GETRESOURCES/GETCONNECTOR(2-pass)/GETENCODER/GETCRTC/GETPLANERESOURCES/GETPLANE; OBJ_GETPROPERTIES/GETPROPERTY/GETPROPBLOB (best-effort); ADDFB/ADDFB2/RMFB/DIRTYFB; **SETCRTC** (legacy modeset); **PAGE_FLIP + readable drm_event_vblank(DRM_EVENT_FLIP_COMPLETE=0x02) off the fd** (essential to schedule frame 2); CURSOR/CURSOR2 (optional — EINVAL → software cursor); CREATE/MAP/DESTROY_DUMB; PRIME_HANDLE_TO_FD/FD_TO_HANDLE/GEM_CLOSE (avoidable if render+scanout = same single node).

### VFS synthetic-file mechanism
- Static `RamEntry` table (servers/vfs/src/lib.rs:~1113) serves /proc synthetic files; ramfs prefix set at :1142 (b"/proc"). This is the insertion point for read-only synthetic /sys — but needs symlink + dir-listing support verified (delegated).


---

# §D. EXECUTABLE DESIGN

This is the implementation plan. Every item cites the exact file, the surface it satisfies,
and the safety constraint. An implementation agent should not need to re-derive anything.

## D0. Guiding architecture decisions

1. **Extend `drivers/src/drm_device_interface.rs` (the live UAPI handler), NOT `drm_driver.rs`.**
   drm_driver.rs uses a bespoke serialize() wire format and made-up cmd numbers — dead for Mesa.
   Reuse from the `drm/` submodule: `auth.rs` (master state), `device.rs`/`framebuffer.rs`/`modes.rs`
   (object model), which drm_device_interface.rs already imports via `use super::drm::*`.
2. **Two consumers over the one card0 fd:** the Mesa/GBM client (buffer ioctls only) and the
   compositor (KMS + master + event read). Implement the full union; both go through the same
   `handle_ioctl` match.
3. **Single card0 node, no separate render node.** Avoids PRIME entirely (render==scanout node);
   dumb buffers are ADDFB'd directly. PRIME_* stubbed to ENOSYS (Mesa tolerates → software).
4. **No new fd classes.** card0 and event* are ordinary `VnodeKind::DynamicDevice` VFS fds; dumb
   buffers are mmap regions, DRM events read from the card0 fd. Existing fd ranges
   (sockets [0x100,0x300), epoll 0x400+, tty 0x1000+) are untouched and stay disjoint by construction.
5. **Lock/user-mem invariant (82d0cc3):** DRM ioctls run synchronously in the caller's AS so raw
   `arg` derefs are legal, BUT the current code derefs user memory while holding `device.lock()`.
   ALL new/edited ioctl arms MUST: (a) copy the entire user input struct into a kernel local BEFORE
   taking `device.lock()`, (b) compute under the lock using locals, (c) drop the lock, (d) copy
   outputs back to user. Never deref `arg` while `device.lock()` (or any spinlock) is held.
   evdev already follows the safe model (with_task_address_space + arch_interrupt_save around
   DEVICES.lock, user writes via write_user_buf) — keep it.

## D1. DRM ioctl additions — `drivers/src/drm_device_interface.rs`

Add constants + match arms in `handle_ioctl` (:252). Restructure each new arm to copy-in before
`device.lock()`. Request codes (authoritative, 64-bit):

| ioctl | code | struct(size) | behavior |
|---|---|---|---|
| GET_CAP | 0xC010640C | drm_get_cap(16) | DUMB_BUFFER(0x1)→1, TIMESTAMP_MONOTONIC(0x6)→1, CRTC_IN_VBLANK_EVENT(0x12)→1, ADDFB2_MODIFIERS(0x10)→0, PRIME(0x5)→0, ASYNC_PAGE_FLIP(0x7)→0; unknown→value 0, Ok (do NOT EINVAL — Smithay best-effort probes) |
| SET_CLIENT_CAP | 0x4010640D | drm_set_client_cap(16) | UNIVERSAL_PLANES(2)→Ok(0); ATOMIC(3)→Err (EINVAL) so Smithay selects legacy; STEREO_3D(1)/others→Ok(0) |
| SET_MASTER | 0x0000641E | none | root single-seat: auth::set_master best-effort, always Ok(0) |
| DROP_MASTER | 0x0000641F | none | Ok(0) |
| GET_MAGIC | 0x80046402 | drm_auth(4) | return a nonzero magic; stub |
| AUTH_MAGIC | 0x40046411 | drm_auth(4) | Ok(0) |
| GEM_CLOSE | 0x40086409 | drm_gem_close(8) | free handle: buddy::free the pages, remove from DUMB_BUFFERS + cookie table; Ok(0) even if unknown |
| DESTROY_DUMB | 0xC00464B4 | {u32 handle}(4) | free dumb buffer (buddy::free + DUMB_BUFFERS remove); Ok(0) |
| ADDFB2 | 0xC06864B8 | drm_mode_fb_cmd2(104) | LINEAR only: use handles[0]/pitches[0]/offsets[0], pixel_format (XR24/AR24), ignore modifier[]; same internal path as std_handle_addfb; return fb_id |
| RMFB | 0xC00464AF | u32 fb_id(4) | device.remove_framebuffer; Ok(0) |
| DIRTYFB | 0xC01864B1 | drm_mode_fb_dirty_cmd(24) | flush fb to host: virtio_gpu transfer_to_host + flush of that fb's resource; Ok(0). If no virtio_gpu, no-op Ok(0) |
| GETPLANERESOURCES | 0xC010_64B5 | drm_mode_get_plane_res(16) | report device.planes (2-pass count/fill); optional |
| GETPLANE | 0xC020_64B6 | drm_mode_get_plane(32) | report plane; optional |
| PRIME_HANDLE_TO_FD | 0xC00C642D | drm_prime_handle(12) | Err(ENOSYS) — no PRIME; Mesa falls back |
| PRIME_FD_TO_HANDLE | 0xC00C642E | drm_prime_handle(12) | Err(ENOSYS) |

Notes:
- Replace the custom GET_CAP (0x1006) usage — Mesa/Smithay use the STANDARD 0xC010640C. Keep 0x1006
  for the legacy DOOM path if anything still calls it, but standard is the one that matters.
- **PAGE_FLIP (already 0xC01864B0):** extend `std_handle_page_flip` — after the scanout flip, if
  `flip.flags & DRM_MODE_PAGE_FLIP_EVENT(0x01)`, enqueue a drm_event_vblank (see D3). This is what
  lets Smithay schedule frame N+1. Preserve the console-disable side effect.
- **ADDFB2/ADDFB present path:** both must, when virtio_gpu is present, create_resource_2d +
  attach_backing(phys) so SETCRTC/PAGE_FLIP/DIRTYFB can transfer_to_host+flush (existing pattern in
  std_handle_addfb:766). For a plain Limine fb (no virtio_gpu) the phys mapping + handle_write path
  scans out directly.
- Dumb-mmap hardening (small): in `handle_ioctl_mmap` (:830) validate `requested_phys` is a value
  present in DUMB_BUFFERS before returning it, else Err. (Prevents arbitrary phys mapping; cookie==phys
  stays.) WC page attribute is deferred (perf-only under softpipe; virtio transfer reads coherently).

Est: ~350 LoC.

## D2. DRM event channel + card0 read/poll — `drm_device_interface.rs` + `servers/drm/src/lib.rs`

- Add a bounded event queue to DrmDeviceInterface (or a global `Mutex<VecDeque<[u8;32]>>`), holding
  serialized drm_event_vblank blobs (type=0x02 FLIP_COMPLETE, length=32, user_data echoed,
  crtc_id, sequence++, tv_sec/usec from CLOCK_MONOTONIC).
- On PAGE_FLIP-with-event: push one event, call `sched::try_wake_poll()`.
- **Delivery timing:** deliver on the 100 Hz console/sched tick (throttle to ~60 Hz), NOT instantly,
  to give Smithay a vblank-like cadence and avoid a busy render loop. (See Risk R2.) Simplest: a
  `drm_tick()` called from the existing periodic tick that flushes at most one queued flip per crtc
  per ~16ms.
- `handle_read` (:854) → return queued events (whole events only, min 8 bytes), else 0/EAGAIN.
- **servers/drm/src/lib.rs:** add a `VFS_READ` arm (currently missing — only VFS_IOCTL/WRITE/CLOSE
  handled). It must copy events to the user buffer via `sched::with_task_address_space(pid, ...)`
  using pid@data[24] (mirror evdev's read), NOT raw deref (read path is not guaranteed same-AS for
  all callers; use the safe path). Return count or -EAGAIN.
- Add a `VFS_POLL` arm to servers/drm that returns POLLIN(0x1) when the event queue is non-empty.

Est: ~200 LoC.

## D3. st_rdev plumbing — `servers/vfs/src/lib.rs`

- Extend `DynamicDeviceEntry` (vfs:~666) with `major: u32, minor: u32`. Change `register_device`
  signature to `(path, port, dev_id, major, minor)` (or add `register_device_rdev`), update the 3
  callers: DRM → (…, 226, 0); evdev event0 → (…, 13, 64); event1 → (…, 13, 65).
- In `handle_fstat`/`stat_common` for DynamicDevice (:5780), pass `makedev(major,minor)` as rdev.
- In `write_stat_full` (:5507) add a `rdev: u64` parameter and write it at the st_rdev offset
  (per-arch: x86_64 offset 40, aarch64 offset 32 — match the existing per-arch layout the function
  already uses for other fields). Default 0 for non-device fds.
- Linux makedev encoding: `(major&0xfff)<<8 | (minor&0xff) | ((minor&~0xff)<<12)`; for 226:0 →
  0xE200, 13:64 → 0xD40, 13:65 → 0xD41.

Est: ~60 LoC. **This gates the whole sysfs contract** — libdrm derives 226:0 from here.

## D4. Synthetic read-only sysfs — `servers/vfs/src/lib.rs`

Model card0 as **platform bus** to AGREE with the libudev shim (which already declares the GPU parent
as subsystem=platform, driver=virtio_gpu — libudev.c:124). Cheaper than PCI (no PCI_SLOT_NAME / hex
attr files). The one requirement libdrm's platform branch imposes: `device/uevent` must carry
`MODALIAS=platform:...` (or OF_FULLNAME). (If a probe shows a consumer needs a PCI id, switch to the
virtio-on-pci model per agent-2 §5 — see Risk R1.)

New infrastructure:
1. **Symlink table** `SysLink{ path: &'static [u8], target: &'static [u8] }` + a `SYS_LINKS` array.
   - Teach `handle_readlink` (:4819): before the RAMFS/-EINVAL fallback, scan SYS_LINKS; on match
     return the target (make_reply with the string). 
   - Teach `stat_common`/`handle_fstat` (:5785) + `handle_getdents64` (:3760): a path in SYS_LINKS
     reports `S_IFLNK|0777` / `DT_LNK`.
2. **Dir allow-list:** add to RAMFS_DIRS: `/sys`, `/sys/dev`, `/sys/dev/char`, `/sys/class`,
   `/sys/class/drm`, `/sys/class/input`, `/sys/devices`, `/sys/devices/platform`,
   `/sys/devices/platform/gpu`, `/sys/devices/platform/gpu/drm`,
   `/sys/devices/platform/gpu/drm/card0`, and the input parents.
3. **gen_sys_content(path)** (mirror gen_proc_system_content :2092), dispatched from handle_open
   for `/sys/...` uevent files:
   - `/sys/dev/char/226:0/uevent` → `MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\n`
   - `/sys/devices/platform/gpu/uevent` → `DRIVER=virtio_gpu\nMODALIAS=platform:virtio_gpu\nOF_FULLNAME=/gpu\n`
   - `/sys/class/input/event1/uevent` → `MAJOR=13\nMINOR=65\nDEVNAME=input/event1\n` (+event0)
4. **Symlinks (SYS_LINKS):**
   - `/sys/dev/char/226:0` → `../../devices/platform/gpu/drm/card0`
   - `/sys/class/drm/card0` → `../../devices/platform/gpu/drm/card0`
   - `/sys/devices/platform/gpu/drm/card0/device` → `../../../gpu` (the parent device dir)
   - `/sys/devices/platform/gpu/subsystem` → `../../../bus/platform`  (basename "platform" = the load-bearing bus match, agent-2 §1C)
   - `/sys/class/input/event1` → `../../devices/platform/gpu/../input/... ` (mirror for input)
5. **Real dir:** `/sys/devices/platform/gpu/drm` must be a listable dir containing `card0` (agent-2:
   `device/drm` is the "is-DRM" gate — drmNodeIsDRM stat + drmGetMinorNameForFD opendir/prefix).
   Serve `card0` as a getdents child of that dir.

Minimum viable subset (if staging): #3 uevent + #4 subsystem symlink + #5 device/drm dir + D3 rdev.
That is exactly what drmGetDevices2's platform branch + drmNodeIsDRM require.

Est: ~400 LoC (symlink infra is the bulk).

## D5. evdev extension — `servers/evdev/src/lib.rs`

1. **Second device:** in `init`, mark DEVICES[1].in_use, `register_device("/dev/input/event1", port_id, 1, 13, 65)`.
   Keep event0=keyboard (13:64). (event2 is the shim's touchscreen slot; we surface the tablet as
   event1=absolute pointer. Coordinate with the shim lane so its ID_INPUT_MOUSE node maps to event1 —
   the "one-line flip" is on the shim side.)
2. **Bump ring:** MAX_EVENTS 64→256 (tablet emits X+Y+SYN per motion; bursts overflow 64). MAX_DEVICES stays 4.
3. **Per-device clockid:** add `clockid: u32` to EvdevDevice (default CLOCK_MONOTONIC=1; keyboard fine
   too). `push_event` stamps `time` from CLOCK_MONOTONIC ns (sched monotonic source) — it already uses
   sched::ticks (monotonic); convert to (sec, nsec/usec) properly.
4. **Rewrite the ioctl decoder** (currently a fragile `cmd & 0xFF` scheme) to decode 'E'-type
   (0x45) ioctls by nr and per-dev capability. Serve, keyed on dev_id:
   - EVIOCGVERSION(nr 0x01) → 0x00010001
   - EVIOCGID(0x02) → input_id{bustype BUS_VIRTUAL 0x06, vendor, product, version} (8B)
   - EVIOCGNAME(0x06,len) → dev0 "QEMU Virtio Keyboard", dev1 "QEMU Virtio Tablet"
   - EVIOCGPHYS(0x07)/EVIOCGUNIQ(0x08) → -ENOENT (empty ok)
   - EVIOCGPROP(0x09,len) → all-zero bitmask (NO INPUT_PROP_DIRECT → stays a pointer, not touchscreen)
   - EVIOCGBIT(0x20+ev,len): ev=0 → dev0 bits (SYN|KEY)=0x03, dev1 (SYN|KEY|ABS)=0x0B;
     ev=EV_KEY(1) → dev0 full keyboard 0xFF (as today), dev1 set only BTN_LEFT(0x110)/RIGHT(0x111)/MIDDLE(0x112) bits;
     ev=EV_ABS(3) → dev1 set ABS_X(0)/ABS_Y(1) bits, dev0 none; ev=EV_REL(2) → none
   - EVIOCGABS(0x40+abs): dev1, abs∈{ABS_X=0,ABS_Y=1} → input_absinfo{value 0, min 0, max 32767,
     fuzz 0, flat 0, resolution 0} (24B). BOTH axes present (libinput rejects X-xor-Y) with equal
     (zero) resolution (libinput rejects mismatched res).
   - EVIOCGKEY(0x18)/EVIOCGLED(0x19)/EVIOCGSW(0x1b) → zeroed bitmask, Ok
   - EVIOCSCLOCKID(0xa0, int) → store clockid, Ok(0)
   - EVIOCGRAB(0x90, int) / EVIOCREVOKE(0x91) → Ok(0) (accept; compositor may grab)
   - FIONREAD(0x541B) → keep
   All user writes via `with_task_address_space(pid, ...)` (pid@arg3), as today.
5. **SYN_REPORT:** the virtio-input driver already forwards SYN events from the device; ensure each
   motion frame ends with EV_SYN/SYN_REPORT(0,0,0) or libinput emits no motion (evdev-fallback.c:988).
6. **Readiness:** generalize `has_key_event` usage — for event1 (pointer) readiness = `has_events(dev_id)>0`
   (any pending event incl ABS/SYN), not the key-only filter. Keep has_key_event for fd0 console only.

Est: ~300 LoC.

## D6. virtio-tablet driver — `drivers/src/virtio_keyboard.rs` (generalize) or new `virtio_input.rs`

- QEMU exposes virtio-keyboard-pci and virtio-tablet-pci as two separate virtio-input PCI functions
  (device id 0x1052). The existing driver binds one. Generalize to enumerate ALL virtio-input
  functions, read each device's config `DEVID`/name (ID string), and assign evdev index:
  keyboard→0, tablet→1. The virtio-input event stream is ALREADY Linux evdev (type/code/value), so
  `evdev_server::push_event(idx, ev.type_, ev.code, ev.value)` works unchanged for ABS/KEY/SYN.
- Poll both queues from the periodic poll (`poll_events`), route by the device's assigned index.
- The tablet reports EV_ABS ABS_X/ABS_Y (0..32767) + BTN_LEFT + EV_SYN — matches D5's advertised caps.

Est: ~150 LoC (mostly refactor of the existing virtio-input logic to be multi-instance).

## D7. QEMU wiring + poll/epoll DynamicDevice arm

1. **`scripts/run-qemu.sh`:** add `-device virtio-tablet-pci` after `virtio-keyboard-pci` in BOTH the
   aarch64 (:192) and x86_64 (:200-207) QEMU_ARGS branches.
2. **`.claude/skills/run-leandros/driver.py`:** add `"-device", "virtio-tablet-pci"` after the two
   virtio-keyboard-pci occurrences (:149, :182).
3. **`servers/vfs/src/lib.rs handle_poll` (:4303):** add a `VnodeKind::DynamicDevice{port,dev_id}` arm
   (replace the (0,0) at :4369) that sends a `VFS_POLL` proxy (dev_id@[0], pid@[24]) to `port` and
   returns its (events, seq). The device servers (evdev D5.6, DRM D2) answer POLLIN + an edge seq.
   Keep this OUTSIDE any EPOLL_INSTANCES lock (the wait loop already drops it before probe — :5988).

Est: ~120 LoC.

## D8. Commit split (7 commits, each builds + boots on both arches)

1. **drm: st_rdev + device-node major:minor** (D3) — smallest, unblocks sysfs; verify stat("/dev/dri/card0").st_rdev==226:0.
2. **drm: full KMS/buffer ioctl surface** (D1) — GET_CAP/SET_CLIENT_CAP/SET/DROP_MASTER/GET/AUTH_MAGIC/GEM_CLOSE/DESTROY_DUMB/ADDFB2/RMFB/DIRTYFB + copy-in-before-lock refactor + mmap validation.
3. **drm: page-flip event channel + card0 read/poll** (D2) — event queue, throttled tick delivery, servers/drm VFS_READ+VFS_POLL arms.
4. **vfs: synthetic read-only sysfs + symlink support** (D4) — symlink table/readlink/stat/getdents + /sys tables + gen_sys_content.
5. **evdev: EV_ABS + full EVIOC surface + event1 + CLOCK_MONOTONIC** (D5).
6. **drivers: multi-instance virtio-input (keyboard+tablet → evdev0/1)** (D6).
7. **vfs+tooling: DynamicDevice poll readiness + QEMU virtio-tablet** (D7).

Total est: ~1580 LoC.

## D9. Test ladder (each rung: RELEASE build, run-leandros driver screenshot, BOTH x86_64 + aarch64)

- **R0 raw-ioctl DRM smoke** (`userland/drmsmoke`, new): open /dev/dri/card0; VERSION; GET_CAP(DUMB_BUFFER)==1
  & TIMESTAMP_MONOTONIC==1; GETRESOURCES (1 crtc/conn/enc, sane min/max); GETCONNECTOR connected + ≥1 mode;
  CREATE_DUMB 256x256 XR24; MAP_DUMB; mmap; fill gradient; ADDFB2; SETCRTC; DIRTYFB; DESTROY_DUMB/RMFB.
  **Accept:** every ioctl returns 0; screenshot shows the gradient full-screen; no serial panic. Also
  assert stat st_rdev==226:0. Gate for commits 1-2.
- **R1 sysfs probe** (`userland/drmenum`, new): drmGetDevices2()-style: stat card0 → 226:0; readlink
  /sys/dev/char/226:0/subsystem→…platform; open /sys/dev/char/226:0/device/uevent has MODALIAS;
  opendir /sys/…/device/drm lists card0. **Accept:** device enumerates; driver name from VERSION printed.
  Gate for commit 4. (If this needs PCI id, flips R1 to the virtio-pci model — Risk R1.)
- **R2 evdev/tablet** (`userland/evtest2`, new): open /dev/input/event1; EVIOCGNAME=="QEMU Virtio Tablet";
  EVIOCGBIT(0)&ABS; EVIOCGABS(ABS_X).max==32767; EVIOCSCLOCKID(MONOTONIC); epoll_wait on the fd; move the
  QEMU pointer via driver.py → observe ABS_X/ABS_Y + SYN_REPORT with monotonic timestamps; epoll wakes.
  **Accept:** ≥1 absolute-motion frame + SYN read after driver-injected motion; epoll idle-CPU ~0 when still.
  Gate for commits 5-7.
- **R3 kmscube/GLES2 (M3 exit):** kms_swrast GBM + GLES2 kmscube (GBM_ALWAYS_SOFTWARE=1) → animated
  frames to card0. **Accept:** screenshot shows a rotating textured cube; ≥3 distinct frames over time.
- **R4 libinput classification:** libinput `list-devices`/`debug-events` on the shims → event1 tagged
  as pointer (after the shim ID_INPUT_MOUSE flip); pointer-motion-absolute events flow on injected motion.
  **Accept:** device listed "Pointer", absolute-motion events logged.
- **R5 anvil (M4 exit):** Smithay anvil kms backend starts via libseat/libudev/libinput shims,
  composites a wl_shm client, cursor tracks the tablet, keyboard types. **Accept:** screenshot shows the
  client window + cursor; cursor moves on injected motion; a typed key reaches the client. Both arches.

## D10. Risks + cheap probes

- **R1 — sysfs fidelity/necessity.** Which consumer actually calls drmGetDevices2/needs a PCI id is
  uncertain; Smithay (drm-rs) reads no sysfs, and Mesa swrast tolerates a missing PCI id. *Probe:* run
  R1 (drmenum) with the platform-modeled minimal sysfs; if a consumer demands a PCI id, switch card0 to
  agent-2's virtio-on-pci model (add PCI_SLOT_NAME + vendor 0x1af4/device 0x1050 hex files). Cheap:
  one on-target run tells you which branch libdrm takes.
- **R2 — DRM event cadence (no real vblank).** Delivering FLIP_COMPLETE instantly may spin Smithay's
  render loop; the M1 idle-CPU criterion is the tripwire. *Probe:* R5 with an idle-CPU meter (reuse
  idletest); if CPU pegs, throttle event delivery to ~60 Hz on the sched tick (D2 already recommends this).
- **R3 — ABS coordinate scaling.** libinput maps device coords via EVIOCGABS max; if our max≠QEMU's
  32767, the pointer is offset/clipped. *Probe:* R2 prints raw ABS max and compares to observed range
  when the pointer is driven to a screen corner.
- **R4 — CLOCK_MONOTONIC discontinuity across EVIOCSCLOCKID.** libinput drops events whose timestamps
  jump when the clock switches. *Probe:* R2 asserts timestamps are monotonic non-decreasing across the
  first post-SCLOCKID events. Mitigation: stamp from the monotonic source from the start.
- **R5 — DIRTYFB / present coherence on virtio-gpu.** If DIRTYFB (or the SETCRTC/PAGE_FLIP transfer)
  doesn't transfer_to_host+flush, CPU-rendered pixels never reach the host display. *Probe:* R0
  screenshot — a black screen with successful ioctls is this bug. Mitigation: ensure every present path
  (SETCRTC, PAGE_FLIP, DIRTYFB) calls virtio_gpu transfer_to_host+flush for the bound fb's resource.
- **R6 — double-binding virtio-input.** The generalized driver must bind BOTH functions and not route
  both to evdev0. *Probe:* boot serial shows two evdev devices; R2 reads event1 (tablet) while event0
  still delivers keys.
- **R7 — copy-in-before-lock regressions.** Editing the existing ioctl arms to copy before device.lock
  risks a page-fault-under-spinlock freeze (82d0cc3 shape) if any deref stays under the lock. *Probe:*
  grep the final drm_device_interface.rs for `arg as *` / `&mut *(arg` occurring after a `.lock()`;
  R0/R5 exercise every arm — a freeze (no panic, pegged CPU, dead Ctrl-C) is the signature.
- **R8 — WC/uncached dumb mapping (perf only).** Deferred; softpipe correctness is unaffected (virtio
  transfer reads the cached mapping coherently). No probe needed; revisit at M7 perf.

## D11. Cross-arch notes
- All DRM/evdev structs are fixed-width and identical x86_64==aarch64 (both LP64). Only drm_version has
  pointer-width fields — already 64 bytes on both; existing VERSION handler is correct.
- st_rdev offset differs by arch in the stat struct (x86_64 40 / aarch64 32) — write_stat_full already
  branches per-arch for other fields; add rdev at the matching offset. This is the one arch-sensitive edit.
- KMS scanout takeover is arch-independent: virtio-gpu 2D (SetScanout/ResourceFlush) over PCI works on
  both; a plain Limine fb falls back to the phys-mapped handle_write path. fb console is disabled on
  first SETCRTC/PAGE_FLIP (framebuffer::set_console_disabled(true)).
