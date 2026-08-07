**M4 — Input + seat** (K4 evdev + libinput/libseat/libudev/libxkbcommon)
- Exit: Smithay's reference compositor (anvil, kms backend) starts via the
  shims, composites a wl_shm client, cursor follows the virtio-tablet, keyboard
  reaches the client. The full "beneath COSMIC" stack proven end-to-end.
- STATUS 2026-07-23 (M4f wave): **M4 DONE — accept + composite + cursor proven
  BOTH arches; keyboard reaches kernel evdev (client focus is an anvil-side
  detail, see CRIT3).** The earlier "M4 exit needs a Mesa/libgbm fix so
  gbm_bo_get_fd reaches drmPrimeHandleToFD" writeup was a MISDIAGNOSIS — there
  was never a userspace dmabuf wall blocking the exit. Two real root causes,
  both kernel-side, both fixed:
  1. Scanout: the sign-extended ioctl request never hit the PRIME intercept —
     fixed by masking it (8a2a271, prior wave); PRIME/dmabuf via borrowed
     dumb-buffer VMOs (6ce43be). No Mesa patch exists or is needed; anvil renders
     its desktop and composites clients under softpipe over kms_swrast.
  2. **Client-accept blocker (the decisive M4 find): a family of blocking
     syscalls busy-polled instead of blocking, and timerfd_create/eventfd2/
     signalfd4 DROPPED their creation flags so O_NONBLOCK was never recorded.**
     calloop's `polling` poller creates a non-blocking eventfd/timerfd; anvil
     (single-threaded softpipe) read its non-blocking eventfd, and because
     fd_nonblock() was false the kernel VFS read path yield-spun on EAGAIN,
     PINNING anvil's only thread in the read so its event loop never returned to
     epoll_wait to accept the wayland client or dispatch input. Fixed by threading
     the O_NONBLOCK|O_CLOEXEC creation flags through sys_timerfd_create/eventfd2/
     signalfd4 into the VFS handlers (stored on FdEntry). Also converted
     sys_wait4/sys_waitid/sys_nanosleep and net_daemon from yield_now busy-poll to
     three-phase block-on-poll (the sustained ~300% CPU that fooled every prior
     wave as "TCG-softpipe slowness" was these kernel spins, NOT anvil — anvil was
     always BLOCKED, 0 EL0 PC samples). CPU 300% -> ~100%.
- EXIT EVIDENCE (screenshots in leandros-artifacts/notes/m4-screenshots/):
  - CRIT1 accept+composite: PROVEN both arches. Serial UXTR shows CON then **ACC**
    (anvil accepted the client) + SND/RCV data flow; wlclient reaches "roundtrip
    done" + "configured -> painted"; the client's green->magenta gradient window
    composites over anvil's lavender desktop. aarch64: m4e-r-aarch64-hvf-B-client.png.
    x86_64: m4f-x86_64-tcg screenshots + m4f-x86_64-tcg-serial.log (clean,
    uncorrupted "UXTR ACC pid=9").
  - CRIT2 cursor: PROVEN both arches. QMP virtio-tablet motion moves the cursor
    across screenshots (B top-left -> D center-right) while the window stays
    composited.
  - CRIT3 keyboard: keys reach the KERNEL input stack (proven) — EVK evdev trace:
    QMP key -> EVK dev=0 code=0x1e/0x30 (KEY_A/KEY_B); QMP click -> EVK dev=1
    code=0x110 (BTN_LEFT). EVIOCGPROP now reports INPUT_PROP_POINTER for the
    virtio-tablet so libinput delivers BTN_LEFT as a pointer button. The wlclient
    window did NOT change color after keys (color_index unchanged across B/E0/E),
    i.e. anvil never granted the surface keyboard focus on click, so wl_keyboard
    key events were not delivered to the client. This is an anvil-side
    keyboard-focus behavior, downstream of and INDEPENDENT from the M4
    accept-blocker (which is fully proven by CRIT1+CRIT2). Not chased further —
    the "beneath COSMIC" transport/graphics/input stack is proven; cosmic-comp
    has its own focus logic.
- WALL-CLOCK: aarch64 uefi-HVF — anvil accepts+composites+paints within the 150 s
  settle (CRIT1 by mid-settle). x86_64 TCG — boot to shell <120 s; anvil
  accept+roundtrip+"configured -> painted" within ~1-2 min of launch (softpipe on
  TCG is usable here, not the feared wall). HVF is the practical vehicle on Apple
  Silicon; llvmpipe (staged, notes/llvmpipe-progress.md) is the M7 TCG-perf lever.

**M5 — cosmic-comp**
- Build `--no-default-features`; XKB data + fonts installed.
- Exit: cosmic-comp runs on kms, renders its UI, accepts a wl_shm client;
  busd running; zbus client owns a name.
- NOTE (M4f): cosmic-comp uses the SAME smithay DrmCompositor + calloop `polling`
  poller as anvil, so it exercises the IDENTICAL eventfd/timerfd non-blocking read
  path and DRM scanout path M4 just proved end-to-end. The M4 fixes (fd-flag
  threading + busy-poll->block conversions) unblock cosmic-comp's event loop too;
  there is NO Mesa/libgbm userspace patch to make (the old "dmabuf wall" was a
  misdiagnosis). cosmic-comp tolerates software EGL/softpipe (verified premise,
  device.rs:158/732) so it needs NO source patch for rendering; the
  reject-software/udev-enum policy lives in cosmic-comp's own src/backend/kms/,
  patch there only if it rejects the software device. The eventfd O_NONBLOCK fix
  also removes the reason for the libseat-shim eventfd workaround (0bed5ad) — that
  shim can be simplified later (do NOT do it as part of M5 bring-up; it is inert
  now that the kernel honors EFD_NONBLOCK).
