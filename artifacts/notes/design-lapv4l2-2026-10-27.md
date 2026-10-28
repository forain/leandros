# design lapv4l2 — V4L2 /dev/video0 + camtest (2026-10-27)

Status: **code written and committed on `lane/lapv4l2` @ 3c927c41** (base main c899c44b, NOT merged).
aarch64 release build clean. Worktree: /Users/forain/code/leandros-lapv4l2.
What a fast-worker still has to do: rebuild (rootfs fix below), run the QEMU probe + regress, build x86_64 INTEL=1,
prove on the laptop, write the lane note, merge.

## Design (as implemented)
- `kernel/src/v4l2.rs` (new, child module of `syscall`, `#[path] pub(crate) mod v4l2;` at kernel/src/syscall.rs:36).
  - `/dev/video0` = dynamic device 81:0 on a port with a **kernel handler** (`ipc::port::register_kernel_handler`,
    same pattern as drivers/src/bt/hcidev.rs). Handler answers VFS_OPEN (ENODEV with no camera, EBUSY if already open —
    exclusive open), VFS_POLL (POLLIN|POLLRDNORM when a frame is ready, POLLERR when not streaming/unplugged; poll key
    `poll_key(poll_class::VIDEO, 0)`), VFS_CLOSE (owner: STREAMOFF + reset).
  - `'V'` ioctls answered in the syscall layer: `v4l2::intercept` at kernel/src/syscall.rs:11230 (right after
    `syncfile_ioctl::intercept`). Subset: QUERYCAP, ENUM_FMT, ENUM_FRAMESIZES, ENUM_FRAMEINTERVALS, G/S/TRY_FMT,
    G/S_PARM, ENUMINPUT, G/S_INPUT, REQBUFS, QUERYBUF, QBUF, DQBUF, STREAMON, STREAMOFF. MMAP memory only; one discrete
    interval per frame (device default). Structs = linux/videodev2.h LP64 (camtest `_Static_assert`s the ioctl numbers
    against zig's real header — they compile, so sizes match).
  - Buffers: buddy blocks (`mm::buddy::alloc(order)`) sized for the largest frame (clamp 64 KiB..8 MiB), max 8, mmap
    offset = `i << 24`; allocated on first REQBUFS and **never freed** (stale user mappings stay safe). Passed to
    `uvc::Stream::start` as `uvc::Buf { ptr: phys_to_virt(phys) }`.
  - mmap: kernel/src/syscall.rs:2945-2952 — for major 81 `v4l2::mmap_phys(open_id, off, len)` gives phys (len checked
    against the block size), then the existing device-mapping path maps it cached; the old DRM/IPC branch is skipped via
    `kind.filter(|_| phys_addr == 0)`.
  - DQBUF: pumps the stream itself in task context, O_NONBLOCK → EAGAIN, else 1 ms sleeps (`sleep_until_ns`) up to 3 s → EIO.
- Tick: `usb::uvc::set_pump/tick` (drivers/usb/src/uvc.rs:404-412, fn-pointer hook), called from
  drivers/src/usb_hid.rs:79 after `isoch::tick()`; `v4l2::pump` try-locks STREAM, polls, wakes with
  `sched::try_wake_poll_key` (IRQ-safe; retried next tick via WAKE_PENDING).
- `sched::poll_class::VIDEO = 26` (sched/src/lib.rs:3384). Perms: `(81, _)` root:video 0660 (servers/vfs/src/lib.rs:12727).
- `crate::syscall::v4l2::init()` at kernel/src/init.rs:90 (after DRM init).
- `userland/camtest/camtest.c` (zig cc, scripts/build-userland.sh:134-136): `camtest [N] [mjpg|yuyv] [WxH] [-o FILE]`,
  O_NONBLOCK + poll + DQBUF, prints `camtest: frame i seq S bytes B fnv XXXXXXXX head ffd8 ...`, writes last frame to
  /tmp/cam.jpg, ends `camtest: SUMMARY PASS|FAIL frames n/N jpeg j distinct d errors e` (PASS = all frames, >1 distinct
  hash, all ffd8 for MJPEG). `camtest --probe`: ENODEV/ENOENT on open → PASS; with a camera checks QUERYCAP/G_FMT and
  that DQBUF fails when not streaming. Added to the regress list (.claude/skills/run-leandros/runtests.py:105).

## Findings
- QEMU aarch64 (HVF) on the first build: `ls -l /dev/video0` → `crw-rw---- 1 root video 81, 0` (node + perms OK).
- `camtest --probe` got RC=127: binary not in the image. FIXED in 3c927c41 by adding `"camtest"` to the install list
  at scripts/mkfs-f2fs-populated.py:429 — **needs a rebuild + re-run** (not yet verified).
- Note: `build-all.sh` exits 0 even when the kernel fails to compile — grep the log for `^error`.

## Remaining steps (fast-worker)
1. `cd /Users/forain/code/leandros-lapv4l2 && git merge main`; `./scripts/build-all.sh --arch aarch64 > log 2>&1; grep -c '^error' log` (must be 0).
2. QEMU: `LEANDROS_RUN_ID=lapv4l2-a64 $SP/lock.sh qemu python3 .claude/skills/run-leandros/runtests.py aarch64 lapv4l2p "/bin/camtest --probe"`
   → expect `camtest: probe: no camera (No such device) PASS`, RC=0. Then the default regress (box farm fine).
3. Laptop: `INTEL=1 ./scripts/build-all.sh --arch x86_64`; under ONE `$SP/lock.sh laptop` hold:
   `python3 scripts/remote.py deploy-kernel --reboot` (trial; do NOT promote), confirm `uname -v` is the trial,
   `remote.py push target/.../camtest /tmp/camtest` (x86_64 userland out dir) + `chmod 755`, `id leandro` must list `video`,
   then `remote.py run "su leandro -c '/tmp/camtest 30'"` (or `sudo -u leandro`).
   Expect: header `camtest: uvcvideo / UVC Camera (UVC 1.00) / usb-xhci-N caps 04000001`, format list MJPG + YUYV,
   `using MJPG 640x480 sizeimage ...@30 fps`, 30 frame lines all `head ffd8` with differing fnv, `SUMMARY PASS frames 30/30`.
   `remote.py pull /tmp/cam.jpg ./artifacts/...` and `file cam.jpg` → `JPEG image data ... 640x480` (do NOT open it).
   Boot `good` back before releasing the lock.
4. If DQBUF times out on the laptop: check klog for `[UVC] slot ..: format 1 frame ..` (Stream::start ran) and whether
   `usb_hid::poll` runs (it is the BSP timer hook); DQBUF pumps by itself, so a timeout means the pipe delivers nothing.
   If mmap fails EINVAL: `len` from QUERYBUF = pool cap (power-of-two pages) — mmap_phys accepts len <= 4096<<order.
5. Lane note `artifacts/notes/lane-lapv4l2-2026-10-27.md`, merge per LANE_PROTOCOL (grep -ci claude == 0), remove worktree.

## Known limits (follow-ups)
Exclusive open (PipeWire probing while another app streams gets EBUSY); single interval per frame size; no USERPTR/DMABUF
(EXPBUF), no controls (QUERYCTRL/G_CTRL → ENOTTY); only camera 0.
