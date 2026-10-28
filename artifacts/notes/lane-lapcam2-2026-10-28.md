# lane/lapcam2 — camtest `mmap: Invalid argument` on the laptop webcam (2026-10-28)

Status: FIXED. Design: artifacts/notes/design-lapcam2-2026-10-28.md.

## Root cause
v4l2 buffers are buddy RAM; the device-mmap RAM policy (7ab8f6b7) refuses a
device token naming RAM unless pinned frames come back. v4l2 never supplied pins,
so sys_mmap returned -22 and logged `[MMAP] device token names RAM but no live pinned buffer`.

## Fix
- `v4l2::mmap_frames` replaces `mmap_phys`: same checks, returns the buffer's frames
  after `pageref::inc_many` (pool blocks are never freed, so the VMA pin can never reach zero).
- `sys_mmap`: `v4l2_pins` becomes `pinned` -> `map_shared_frames`. Policy file untouched, BACKEND_MMAP_PIN not used.
- `uvc.fake=1`: fake camera (`Camera.func` is now `Option<UsbFunction>`, `Stream::start_fake`):
  MJPG + YUYV 640x480, ~33 fps from the 10 ms USB tick, embedded 6.7 KB JPEG (trailing pad varies per frame).
- `camtest --mmap`: per-buffer mmap, pattern write/read, bad-offset EINVAL checks.

## Evidence
- QEMU x86_64, unfixed kernel: `camtest: mmap: Invalid argument` + `[MMAP] device token names RAM ... 7CB00000`.
- QEMU x86_64 and aarch64, fixed, `uvc.fake=1`: `camtest mmap: PASS (4 buffers)`, `camtest 30` 30/30 JPEG, distinct 30, /tmp/cam.jpg 6755 B; `camtest yuyv 10` PASS (614400 B).
- Real laptop (trial 701ec053c4bb, as leandro): `camtest 30` PASS 30/30 jpeg 30, ~10 fps (100 ms frame spacing),
  cam.jpg 44452 B; `camtest yuyv 30` PASS 30/30, cam.raw 614400 B; 3 repeats PASS (last JPEG 54540 B, `file`: JPEG 640x480);
  0 `[MMAP]` refusals in the logs. Laptop returned to `good`; not promoted.

## Open
- Real camera delivers ~10 fps at the default MJPG interval (likely auto-exposure / device rate, not investigated).
- camtest must be pushed to the laptop separately (deploy-kernel swaps kernel+initrd only).
