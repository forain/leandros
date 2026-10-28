# lane/lapmt - touchpad phase 2a: evdev multitouch plumbing (2026-10-28)

No I2C-HID input driver yet; this is only the evdev side the driver (phase 2b/2c) will feed.

- `DEVKIND_TOUCHPAD` (8, hot-plugged by a driver, BUS_I2C 0x18): ABS_X/Y 0..1273/0..672, ABS_MT_SLOT 0..4,
  TRACKING_ID 0..65535, MT_POSITION_X/Y (res 12/mm, placeholder until HID physical extents are parsed),
  TOOL_TYPE 0..2; keys BTN_LEFT/TOUCH/TOOL_FINGER/DOUBLETAP/TRIPLETAP/QUADTAP/QUINTTAP; PROP POINTER|BUTTONPAD.
- `DEVKIND_UINPUT_TOUCHPAD` (9): uinput devices that set ABS_MT_SLOT + ABS_MT_POSITION_X; slot count from
  `UI_ABS_SETUP(ABS_MT_SLOT).max + 1`. New `UI_SET_PROPBIT` (nr 110), reported by EVIOCGPROP.
- Per-device slot table (`MT` in servers/evdev/src/lib.rs) updated by `push_event` for every ABS_MT_* event:
  [tracking id, x, y, tool]. Drivers call `push_event` as usual; `mt_init(dev, n)` runs at register.
- `EVIOCGMTSLOTS` (nr 0x0a): `{u32 code; s32 values[n]}`, n = min((len-4)/4, slots); codes TRACKING_ID/X/Y/TOOL_TYPE, else EINVAL.
- **Bug found:** EVIOCGABS was only dispatched for nr 0x40..0x60, so every ABS_MT axis (0x6f..) fell to ENOTTY.
  Range is now 0x40..0x80 (ABS_MAX 0x3f). libinput/libevdev would have rejected any MT device before this.
- libudev shim: hotplug kind `touchpad` -> C_TOUCHPAD fallback; the capability probe already classifies
  BTN_TOOL_FINGER + !INPUT_PROP_DIRECT as ID_INPUT_TOUCHPAD (verified by reading, not by libinput in QEMU).
- Test: `uinputtest` now creates a 5-slot touchpad (06cb:82f1 shape) and checks id/prop/keybits/absbits/EVIOCGABS,
  EVIOCGMTSLOTS (idle -1, two-finger scroll ids/x/y, short buffer, bad code), tap down/up, node removal.

Limits: libinput was not run in QEMU (no libinput CLI in the image); touchpad node classification by libudev is untested end to end.
Not done (later phases): resolution from HID physical extents, tool-type emission, BTN_TOOL_* synthesis (driver side).
