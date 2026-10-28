# lane/lapcam3 - webcam AE priority + exposure controls (2026-10-28)

STATUS: PARTIAL - code done and QEMU green; laptop fps numbers pending (laptop lock queue).

- uvc.rs: parse camera terminal (0x0201) and processing unit controls; ctrl_get/ctrl_set (GET_INFO bit1 checked);
  `apply_controls` before the isoch pipe opens: AE priority default 0 (`uvc.ae_prio=0|1|dev`),
  `uvc.exposure=auto|manual:<100us>`; read-back log line; `no camera terminal` on the fake camera.
- v4l2.rs: QUERYCTRL/G_CTRL/S_CTRL for EXPOSURE_AUTO/ABSOLUTE/AUTO_PRIORITY; S_PARM stores the requested interval and
  Stream::start negotiates it (not streaming only).
- camtest: `--fps`, `--ae-prio N`, `--exposure N` (manual, 100us units).
- QEMU `uvc.fake=1`: aarch64 and x86_64 camtest (default and `--exposure 50 --ae-prio 0`) 30/30 and 20/20 PASS, JPEG ok.
- Host tests: usb crate cannot be built for the host (depends on sched asm); a parse unit test is in uvc.rs
  but not runnable on the host.
- Note: x86_64 image build needed lane/lapgreet's pam-leandros lib.rs overlaid temporarily (shared greetd srchash).
- Laptop script: scratchpad lapcam3-laptop.sh (deploy trial, push camtest, T1/T2 runs, discard + reboot to good).
