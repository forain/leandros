# Lane wheel — mouse-wheel scrolling (2026-10-03)

Branch `lane/wheel`, three commits on top of `f994928`:
- `249c185` evdev, virtio-input: advertise the tablet's scroll wheel
- `8ecd48e` evtest2: check the wheel capability and count wheel events
- tooling: `driver.py wheel`, `ffsession.py --wheel`

## Root cause

The wheel events were never lost on the way in. QEMU's virtio-tablet sends
each detent as `EV_REL/REL_WHEEL ±1` + `SYN_REPORT`. `drivers/src/virtio_keyboard.rs`
forwarded every event unfiltered, and `servers/evdev` queued them on
`/dev/input/event1`. The break was capability advertisement:
`eviocgbit()` reported `EV_SYN|EV_KEY|EV_ABS` (0x0B) and an empty EV_REL bitmap.
libevdev, inside libinput 1.27.1, drops every event whose type/code the node
did not advertise. So libinput never built a pointer-axis event, cosmic-comp
never sent `wl_pointer.axis`, and nothing scrolled. Nothing else on the path
needed changing: the libudev shim hardcodes `ID_INPUT_MOUSE` for event1, and
libinput's wheel code runs for any POINTER-capable device.

## Fix

- The driver reads the device's own `EV_BITS` bitmaps for EV_REL and EV_KEY
  from virtio-input config space at probe and passes them to
  `evdev_server::set_device_caps()`.
- evdev mirrors the wheel axes (REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES,
  REL_HWHEEL_HI_RES, and never REL_X/Y) and the BTN_LEFT..BTN_TASK byte. When
  any REL bit is present it sets EV_REL in EVIOCGBIT(0).
- Why mirror the device instead of hardcoding: libinput ≥ 1.19 goes hi-res-only
  when REL_WHEEL_HI_RES is advertised. Claiming it on a device that never sends
  it would break scrolling again. QEMU 11.1.1 reports `EV_REL=0x100` (REL_WHEEL
  only, so libinput synthesises v120 from it) and `BTN byte=0x1F`. That byte
  adds BTN_SIDE/BTN_EXTRA (back/forward), which were previously dropped the same
  way.
- **HVF landmine found along the way.** The first build aborted QEMU at boot on
  the Mac with `hvf_handle_exception: Assertion isv failed`. Once inlining
  changed, LLVM compiled the existing `read_volatile(device_cfg.add(2))` in
  `device_supports_ev_abs` as `ldrb w8, [x19, #2]!`. A writeback access traps
  with ISV=0, and HVF cannot emulate it. Every virtio-input device-config byte
  access now goes through plain `ldrb`/`strb` asm (`cfg_read8`/`cfg_write8`).
  Other drivers' `read_volatile` MMIO has the same latent exposure. It only
  bites when codegen picks a writeback form.

## Evidence

Mac, QEMU 11.1.1. aarch64 ran on HVF and x86_64 on TCG. The linux desktop and
laptop were unreachable (no route, Tailscale timeout), so x86_64/KVM was not run.

| check | aarch64 | x86_64 |
|---|---|---|
| boot log `[INPUT] tablet EV_REL bits=0x100 BTN byte=0x1F` | yes | yes |
| evtest2 caps (EV_REL, REL_WHEEL, no REL_XY) | PASS | PASS |
| evtest2 + QMP wheel: `wheel_up=3 wheel_down=1 wheel_frame PASS` (values +1/−1) | yes | up=3, PASS |
| cosmic-term `seq 1 400`, wheel up ×15 | 383–400 → 293–311 | 373–400 → 283–311 |
| Firefox 300-row page, wheel down ×10 / up ×5 | Row 1 → Row 46 → Row 26 | Row 1 → Row 46 → back |
| click control (cosmic-term View menu opens) | yes | yes |
| evsplit 60 injected moves, two readers | BROADCAST 60/60 | — |
| evtest2 abs motion frames | PASS | — |
| runtests `--suite regress` (14 suites) | all RC=0 | all RC=0, overall PASS |

Screenshots and `wheel.json` are in `lane-wheel-files/`, with prefix `a64b-` or
`x64b-`. Whole runs: `/tmp/wheel-ff/run-{a64b,x64b}`.
Reproduce with `LEANDROS_RUN_ID=wheel LEANDROS_VNC_PORT=5962 ffsession.py <arch> <tag> --wheel --wait 10`.
Scrolling 15 detents moves the terminal 90 lines, which is cosmic-term's 6 lines
per detent, so v120 synthesis works.

## Gaps

- x86_64 under KVM is untested. The linux machines were unreachable. The change
  has no arch- or accelerator-specific part except the aarch64 asm.
- Horizontal wheel: QEMU 11.1's virtio-tablet advertises no REL_HWHEEL, so it is
  mirrored but cannot be exercised. QMP `wheel-left/right` produced no events.
- The virtio-tablet is the only pointer source feeding evdev today. If a USB HID
  mouse is ever wired to `push_event`, its driver must call `set_device_caps`
  too.
- The terminal's changed-pixel fraction is small (~0.2 %) because only glyph
  pixels change. Read the screenshots, not that number.
