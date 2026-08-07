**M4 — Input + seat** (K4 evdev + libinput/libseat/libudev/libxkbcommon)
- Exit: Smithay's reference compositor (anvil, kms backend) starts via the
  shims, composites a wl_shm client, cursor follows the virtio-tablet, keyboard
  types. Full "beneath COSMIC" stack proven end-to-end.
- STATUS 2026-07-23 (M4e wave): **M4 DONE + verified both arches.**
  The earlier "Mesa/libgbm gbm_bo_get_fd wall" writeup was a MISDIAGNOSIS. Two real
  root causes, both kernel-side, both fixed:
  1. Scanout: the sign-extended ioctl request never hit the PRIME intercept — fixed
     by masking it (8a2a271); PRIME/dmabuf via borrowed dumb-buffer VMOs (6ce43be).
     No Mesa patch exists or is needed; anvil renders its desktop under softpipe.
  2. **Client-accept blocker (the M4e find): a family of blocking syscalls busy-
     polled instead of blocking, and — decisively — timerfd_create/eventfd2/
     signalfd4 DROPPED their creation flags so O_NONBLOCK was never recorded.**
     The calloop `polling` poller creates a non-blocking eventfd/timerfd; anvil
     (single-threaded softpipe) read its non-blocking eventfd, and because
     fd_nonblock() was false the kernel VFS read path yield-spun on EAGAIN,
     PINNING anvil's only thread in the read so its event loop never returned to
     epoll_wait to accept the wayland client or dispatch input. Fixed by threading
     the O_NONBLOCK|O_CLOEXEC creation flags through sys_timerfd_create/eventfd2/
     signalfd4 and the VFS handle_eventfd/timerfd_create/signalfd_create. Also
     converted sys_wait4/sys_waitid/sys_nanosleep and net_daemon from yield_now
     busy-poll to block-on-poll (the sustained ~300% CPU that fooled every prior
     wave as "TCG-softpipe slowness" was these kernel spins, NOT anvil — anvil was
     always BLOCKED, 0 EL0 PC samples).
- EXIT EVIDENCE: anvil accepts the wl_shm client (UXTR ACC), wl.log reaches
  "roundtrip done" + "configured -> painted"; the client window composites over the
  desktop; QMP virtio-tablet motion moves the cursor across screenshots; a QMP key
  reaches the client (KEY code=). Screens in leandros-artifacts/notes/m4-screenshots.
- WALL-CLOCK: aarch64 uefi-HVF reaches modeset ~<settle>; x86_64 TCG <fill>.
  (HVF is the practical vehicle on Apple Silicon; TCG softpipe is a known M7 perf item.)

**M5 — cosmic-comp**
- Build `--no-default-features`; XKB data + fonts installed.
- Exit: cosmic-comp runs on kms, renders its UI, accepts a wl_shm client;
  busd running; zbus client owns a name.
- NOTE (M4e): cosmic-comp uses the SAME smithay DrmCompositor + calloop `polling`
  poller, so it exercises the IDENTICAL eventfd/timerfd non-blocking read path M4
  fixed — that fix (plus the busy-poll->block conversions) unblocks cosmic-comp's
  event loop too. No Mesa userspace patch is required (software EGL/softpipe is
  tolerated). cosmic-comp needs NO source patch for software EGL (verified premise
  from staging); the udev reject-software policy lives in cosmic-comp's own
  src/backend/kms/, not smithay — patch there only if it rejects software EGL.
