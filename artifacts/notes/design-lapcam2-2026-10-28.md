# lapcam2 — camtest `mmap: Invalid argument` on the laptop (main e590b436)

## Root cause
Not in v4l2 itself: `v4l2::mmap_phys` (kernel/src/v4l2.rs:204-213) accepts the call
(offset 0 = buffer 0, `i < nreq`, `len <= 4096<<order`; the length is page-rounded
in sys_mmap at kernel/src/syscall.rs:2847, and the pool cap is already a power-of-two
page count, so a non-page-aligned MJPG sizeimage is harmless; the sizeimage is never 0 because of the
`max(4096)` and `clamp(64K, 8M)`; MAP_SHARED/PROT_WRITE are not checked).

It is the device-mmap RAM policy added by **7ab8f6b7 (openbugs #95, 2026-10-26)**,
which landed one day BEFORE the v4l2 node (3c927c41, 2026-10-27), and the v4l2 lane
never got a pin:

- kernel/src/syscall.rs:2946-2949: v4l2 sets `phys_addr` = a **buddy block** (RAM).
- kernel/src/syscall.rs:3048-3051: `pinned = drm_device_interface::mmap_pin_frames(...)`
  -> `None` (not a DRM BO).
- `win_pin` -> `None`; `ram = mm::buddy::is_ram(phys, len)` = true.
- drivers/src/device_mmap_policy.rs `decide(NotGuestRam, NotWindow, ram=true)` = **Refuse**
  -> kernel/src/syscall.rs:3075 `return -22` (EINVAL), logging
  `[MMAP] device token names RAM but no live pinned buffer <phys>` (this line should be in
  the laptop dmesg and confirms the diagnosis without a rebuild).

QEMU never reaches it: no UVC device, so VFS_OPEN gives ENODEV before any mmap.

## Intended diff (fast-worker)
1. `kernel/src/v4l2.rs`: change `mmap_phys` to return the pinned frames:
   ```rust
   /// mmap of /dev/video0: buffer `off>>24`, pinned page by page for the VMA.
   pub(super) fn mmap_frames(open_id: u32, off: usize, len: usize) -> Result<Vec<usize>, isize> {
       ... same checks (owner, OFF_SHIFT alignment, i < nreq, i < p.n, 0 < len <= 4096<<order) ...
       let base = p.phys[i];
       let frames: Vec<usize> = (0..len / 4096).map(|k| base + k * 4096).collect();
       mm::pageref::inc_many(&frames);   // under POOL lock
       Ok(frames)
   }
   ```
   Safety argument: pool blocks are never freed (leaked on purpose, v4l2.rs:16-20,301),
   so the pool is the implicit owner (refcount 1 = untracked); the VMA's pin takes it to 2,
   and munmap/exit/fork use `unref_or_free`/inc, which goes back to 1 and never frees.
   Update the module doc (line 9) to say mmap is answered by `mmap_frames`.
2. `kernel/src/syscall.rs` ~2945: keep `let mut v4l2_pins: Option<Vec<usize>> = None;`; in the
   VIDEO_MAJOR branch, on Ok(f) set `phys_addr = f[0]; v4l2_pins = Some(f);`.
   At ~3048 make `pinned` = `v4l2_pins.take().or_else(|| match kind { ... mmap_pin_frames ... })`
   so a v4l2 token goes `Pin::Pinned -> MapPinned -> map_shared_frames`. The existing
   `release_pins` path covers failure. No change to device_mmap_policy.rs.
   (Do not use `BACKEND_MMAP_PIN`: that is DRM's single hook slot.)
3. Optional hardening: in the VIDEO_MAJOR branch, a v4l2 refusal must never fall through to
   the generic device path (it already returns early; keep it so).

## QEMU-reproducible test
QEMU has no UVC device, so add a **fake camera** behind a cmdline knob:
- `drivers/usb/src/uvc.rs`: `uvc.fake=1` (parsed in `set_cmdline`) registers one `Camera`
  whose `info` comes from `parse()` on an embedded VideoStreaming descriptor blob with
  MJPG 640x480 (`dwMaxVideoFrameBufferSize` = 614400 = 150 pages, so this exercises the
  non-power-of-two to power-of-two path) and YUYV 640x480. Its `func` gets a sentinel
  slot (e.g. 0xFF) and `Stream::start` returns Err(EIO) for it (phase A); phase B, if it
  is cheap: a synthetic Stream whose pump copies an embedded ~1 KiB JPEG into queued bufs, so
  the full `camtest 30` path runs in QEMU.
- `userland/camtest`: add `--mmap` mode: open, S_FMT MJPG 640x480, REQBUFS 4, then for each
  buffer QUERYBUF and mmap(PROT_READ|PROT_WRITE, MAP_SHARED) at `m.offset`, write and read
  back a pattern, munmap; also check that mmap at a bad offset (1<<24 * nreq, or offset 4096)
  -> EINVAL, and that mmap after close of a second open -> EBUSY. It prints `camtest mmap: PASS`.
- regress: run `camtest --mmap` on an image booted with `uvc.fake=1` (aarch64 per lane
  rule; both arches at merge). Before the fix it prints `mmap: Invalid argument` and dmesg has the
  `[MMAP] device token names RAM` line; after the fix it prints PASS. Also check the refcount
  returns to 1: after munmap, `pageref::any_tracked(pool block)` is false (a debug /proc
  line or a selftest assertion).
- Keep the uapi host test in drivers/uapi-tests unchanged (policy is unchanged).

## Laptop test plan
1. Before deploying: `dmesg | grep 'device token names RAM'` from the failed run, to confirm.
2. laptop-deploy-kernel with the fix (kernel only; camtest is unchanged unless `--mmap` is
   added, then push camtest via laptop-remote-debug).
3. As leandro: `camtest --probe`; `camtest 30` -> expects MJPG 640x480@30, 30 frames DQBUF'd,
   writes /tmp/cam.jpg. Pull it to the Mac; check `file cam.jpg` = JPEG 640x480 and that
   the size is > 10 KB (not all-zero frames); view one image only if the hash/size checks are not clear
   (screenshots are costly).
4. `camtest yuyv 30` (-> /tmp/cam.raw, size 614400) for the isoch YUYV path; repeat camtest 3 times
   (stale-mapping/realloc path; pool reuse); dmesg must show no `[MMAP]` refusals.
5. Likely next failure points if mmap passes: DQBUF timeouts (isoch alt selection / the
   bandwidth for alt with 7 alts), MJPG frames with zero bytesused; capture `[UVC]` stats.
