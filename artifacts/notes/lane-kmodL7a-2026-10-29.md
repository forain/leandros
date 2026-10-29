# lane kmodL7a (2026-10-29): uvc-adapt, Y-only refactor + kapi exports

Branch lane/kmodL7a. (Status and evidence: see the end of this file.)

- v4l2 ops table: `kmod_abi::KCameraOps` (11 `extern "C"` ops: info/format/frame/stream_start/stop/poll/ready/gone/qbuf/dqbuf/control), `KCamInfo/KFormat/KFrame/KCamBuf/KCamDone`, `KFMT_*`. `kernel/src/v4l2.rs` no longer names any `usb::` type except the camera sink: cameras arrive through `register_camera(owner, ops, ctx)`, which snapshots formats/frames once. `unregister_camera(ctx)` on unplug.
- Built-in uvc registers through it: `usb::uvc::UVC_OPS` over a leaked `CamCtx { Arc<Camera>, Mutex<Option<Stream>> }`; `uvc::set_camera_sink` installs the kernel sink and replays cameras announced earlier (fake camera, early USB).
- Stream pump: the IRQ pump uses `stream_poll` (try-lock inside the adapter), as before. Exposure controls go through `KCameraOps::control` (CIDs), logic unchanged.
- USB class registry (`drivers/usb/src/class.rs`): id entries (vid/pid wildcard 0xffff, class<<16|sub<<8|proto under a mask), static built-in table (video streaming -> uvc), runtime list, `wants()` (xhci uses it instead of `uvc::has_video_streaming` to decide SET_CONFIGURATION), `offer()` at `adopt`, an `UNCLAIMED` list replayed to drivers registered later, `detach()` replaces `uvc::detach`.
- Tick: `usb::class::tick_register/tick` replace `uvc::set_pump/tick`; `usb_hid::poll` calls `usb::class::tick()`. Tick list is read with try_lock and a copy of up to 8 callbacks.
- kapi (ksymtab 33 -> 36): `kapi_v4l2_register`, `kapi_tick_register`, `kapi_usb_class_register`; kmod-kapi shims `v4l2::register`, `tick::register`, `usb::class_register`. `KUsbIf` (24 B), `KUsbProbeFn`, `KTickFn` in kmod-abi.
- ktest `kmodkapi` extended (`kmodkapi_l7a`): camera register/unregister + registry entry, tick callback, class register errors, `wants` on synthetic configs.

Not done here (still L7b): `kapi_usb_ctrl`, `kapi_usb_set_alt`, the isochronous open/submit/reap/close handles, and the USB alias autoload hook at interface enumerate. `KUsbIf.handle` is the slot for now; claimed functions are parked in `exports::CLAIMED`.

## Status and evidence

DELIVERED (Y-only). Gates:
- aarch64 default regress PASS (includes `/bin/camtest --probe`, no camera).
- Webcam path with the fake camera (`uvc.fake=1`, cmdline patched into the Limine ESP copy): `camtest --probe`, `camtest 8` (MJPG 640x480) and `camtest 5 yuyv 640x480` pass on aarch64; `camtest --probe` and `camtest 3` pass on x86_64 (33 frame lines in the log). The 0.6 s / 20.6 s timing pattern of two consecutive `camtest 8` runs is identical on main (harness artifact).
- ktest=kmodkapi PASS on aarch64 and x86_64 (ksymtab 36, the new l7a checks).
- Symbol diff aarch64: the main reference binary (target/final-aarch64/kernel in the main checkout) predates the kmod ktest items, so the diff also shows kmod noise; the lane-specific part is: new `usb::class::*`, `uvc::op_*`/`CTXS`/`SINK`/`class_probe`/`class_detach`, `kapi_v4l2/tick/usb_class_register`, ktest l7a items, v4l2 `Camera` types; gone: `uvc::PUMP`, `has_video_streaming` (inlined into class), `Stream::start_fake` (inlined).

Could NOT be tested: a real UVC camera (QEMU has no usb-host passthrough here), so the real xhci isochronous path, hot-unplug (`uvc::detach` -> `unregister_camera`) and `class::offer` with a real `UsbFunction` are covered only by code reading. The real laptop webcam check is the orchestrator's.
