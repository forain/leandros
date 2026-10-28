# lane/lapuvc — laptop webcam (UVC over xHCI isochronous IN), 2026-10-27

Status: **PARTIAL** — kernel half DONE and proven on the real laptop; the V4L2 `/dev/video0`
surface and `camtest` are NOT built yet (next steps below).

## What landed
- `drivers/usb/src/xhci/isoch.rs`: `IsochPipe::open_ex(.., ntds, framed)` — up to `NTDS_MAX` = 240 TDs
  per pipe (a 125 µs HS µframe stream needs > 80 to ride over a 10 ms tick) and a *framed* IN FIFO
  (`[len u16][data]` per TD, read with `read_packet`) because every UVC payload carries its own header.
  A full framed FIFO drops the new packet (counted missed). `open()` is unchanged for audio.
- `drivers/usb/src/uvc.rs` (new): VC/VS descriptor parser (MJPEG, uncompressed YUY2/NV12, frame sizes,
  isoch alt settings), VS_PROBE/VS_COMMIT (26/34/48-byte control by bcdUVC), smallest alt whose
  bandwidth holds dwMaxPayloadTransferSize, payload reassembly (FID toggle / EOF / ERR), V4L2-style
  buffer queue (`Stream::qbuf` / `dqbuf`, caller-owned buffers, no allocation after `start`, poll is
  try-lock only), camera registry (`uvc::camera(i)`), hot-unplug `detach`.
- Enumeration: a configuration with a VideoStreaming interface is SET_CONFIGURATION'd (same path as
  audio) and handed to `uvc::register`, which logs the descriptors (`[UVC] …`, the lsusb -v equivalent).
- `uvc.selftest=<N>` (kernel cmdline): streams the first camera's MJPEG default for N frames at
  enumeration and logs size + FNV-1a per frame. Off by default; without it the driver only parses and logs.

## Real hardware (HP EliteBook 840 G5, trial lapuvc1, `--append uvc.selftest=30`)
Camera 0408:5371 (high-speed): UVC 1.00, VS iface 1, YUYV (640x480, 320x180, 320x240, 424x240, 640x360)
and MJPEG (those + 848x480, 960x540, 1280x720), all 30 fps; 7 isoch alts on ep 0x81, 128..3072 B/µframe.
Negotiated MJPEG 640x480 → max_payload 3072 → alt 7 (1024 x 3, burst 2, 240 TDs).
30/30 frames, all start with JPEG SOI `ffd8`, sizes 6852 → 27712 B as auto-exposure ramps, checksums
differ frame to frame (e.g. 143cb891, c01122f9, …, 9681973c); payloads 16032, dropped 0, device
errors 0, isoch missed 0, underrun 0. Logs: artifacts/remote-logs/20261027-222111-172.16.149.179/klog-all.txt.

## Next steps (V4L2 + camtest)
1. Device node: `/dev/video0` (char 81:0) in the VFS device list (servers/vfs/src/lib.rs, like /dev/dri),
   with open/ioctl/mmap/poll routed to the kernel the way PRIME/sync-file ioctls are answered in the
   syscall layer (kernel/src/syscall.rs `sys_ioctl`, pattern: kernel/src/syncfile_ioctl.rs). Put the
   V4L2 code in a new `kernel/src/v4l2.rs`.
2. ioctls (64-bit struct layouts from linux/videodev2.h): QUERYCAP (CAPTURE|STREAMING), ENUM_FMT
   (MJPG, YUYV), ENUM_FRAMESIZES/INTERVALS, G/S/TRY_FMT (map width/height → uvc format/frame index),
   REQBUFS (allocate N page-aligned physically contiguous buffers of max_frame, pass `uvc::Buf`s to
   `Stream::start`), QUERYBUF (mmap offset = index << 24), QBUF/DQBUF (→ `qbuf`/`dqbuf`; bytesused,
   sequence, timestamp, V4L2_BUF_FLAG_ERROR), STREAMON/OFF (`Stream::start`/`stop` in task context).
3. Drive `Stream::poll` from the USB tick (`usb::xhci::isoch::tick` via drivers/src/usb_hid.rs) with
   try-lock on the stream, and wake DQBUF/poll waiters when `ready() > 0`.
4. `userland/camtest`: open /dev/video0, S_FMT MJPEG 640x480, 4 mmap buffers, grab N frames, print
   size + FNV-1a (same hash as the self-test for comparison), optional `-o` to save one JPEG.
5. Test: x86_64 QEMU has a usb-host passthrough only; the laptop is the real test
   (`camtest 30` sizes/checksums must vary; compare with `uvc.selftest`).
