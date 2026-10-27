# Lane usbisoch (2026-10-27): xHCI isochronous transfers

Lane 2 of `lane-pisound-design-2026-10-26.md`. **DELIVERED.**

## What
- `drivers/usb/src/xhci/isoch.rs` (new):
  - Isoch TRBs (`TRB_ISOCH = 5`) with SIA on every TD, so the Frame ID is never used. TBC/TLBPC
    follow Linux (on xHCI 1.0+: 0 / TDPC-1 for FS/HS, the burst split for SS).
  - Endpoint contexts: `EP_TYPE_ISOCH_OUT/IN`, CErr 0, interval from `interval_field(.., true, bInterval)`,
    Max ESIT = mps × (HS extra transactions + 1), or the SS companion's `wBytesPerInterval`.
  - `IsochPipe`: 64 one-interval TDs stay queued. `poll()` is tick-safe: try-lock only, no waits,
    no allocation. A byte FIFO (`write`/`read`, no lock) feeds the TDs. Each OUT TD carries
    `rate × interval` frames, accumulated exactly, so 44.1 kHz alternates 44 and 45 frames.
    `done_bytes()` gives the play/capture position. Also `in_flight()`, `queued()`, `pause()`,
    and `open()`/`close()` for task context (Configure Endpoint add/drop, SET_INTERFACE).
  - Errors: Ring Underrun/Overrun leaves the endpoint running, and the next doorbell plus SIA
    re-anchors it. TDs complete in ring order, so a TD older than a completed one counts as
    missed, even when it got no event (Missed Service). Per-TD errors are counted and the stream
    goes on. Counters: `[usbisoch]` line in the iostat window
    (`isoch_underrun`, `isoch_missed`, silence, errors).
  - OUT padding: a TD is padded with silence only when the FIFO is dry and fewer than 16 TDs are
    in flight. An idle or paused stream queues nothing, and its ring draining is not counted as
    an underrun.
  - Cache maintenance: each TD has its own cache-line-aligned power-of-two slot. OUT is cleaned
    after the fill. IN is cleaned before queueing and invalidated before the read. The new
    `DmaBuf::sync_range_for_device/cpu` cover only that slot. No RP1 files touched.
  - Class-driver hooks for uac1: at enumeration, a function with an audio streaming interface
    gets SET_CONFIGURATION. It becomes a `UsbFunction` (raw cfg descriptor, `isoch_endpoints()`,
    `control_in/out`, `set_interface`), registered via `isoch::take_audio()`. `detach` runs on
    unplug.
  - Self-test: `usb.isochtest=<ms>` streams a 1 kHz sine (48 kHz S16 stereo) from the tick to
    the first audio OUT endpoint, after a UAC1 SET_CUR sampling frequency.
- Not done (left for uac2 / later): SS isoch Mult > 0, explicit feedback endpoints, and the
  TT check for an FS audio device behind a HS hub (the existing hub code sets TT fields at
  enumeration; untested for isoch).

## Evidence
- `runtests.py <arch> usbisoch` (host:usbisochtest.py) on aarch64 (Mac, HVF):
  `selftest done: 384000 B in 2000 TDs, underrun 0, missed 0, silence 0, errors 0`.
  WAV: one 2.000 s segment, L/R 1000.0 Hz, RMS 10633, 0/2004 all-zero 1 ms windows.
  Unplug with the pipe open: `selftest: device gone`, then `slot 1: released`, no fault.
- The guard fails on a broken scheduler: the first version primed 64 ms of TDs from a 40 ms
  FIFO. The test failed with 988 Hz, 1.86 % dropout windows and 25 silence TDs.
- x86_64 (box, KVM): `runtests x86_64 host:usbisochtest.py` PASS.
- Bulk/HID: usbstalltest PASS and usbunplugtest PASS (aarch64). Full default regress on the box:
  49/49 suites PASS on x86_64 and on aarch64 (code as merged).
