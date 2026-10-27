# Lane bthid: Bluetooth HID host (gamepad first), 2026-10-27

Status: DELIVERED (software path proven in QEMU + host tests; no radio test, since no BT hardware in QEMU).

## What
- `userland/bthid` (daemon, root, started by init together with btd once `/dev/hci0` exists; `bt=0` / `bthid=0` disable):
  btd client (BT_SEAM section B framing), own SDP client on PSM 1 (PnP 0x1200 -> vendor/product, HID 0x1124 ->
  HIDDescriptorList 0x0206, with continuation), HIDP control 0x11 + interrupt 0x13, outgoing connect and incoming
  reconnect (LISTEN 0x11/0x13), descriptor cache `/var/lib/bluetooth/hid-<ADDR>`, device list `/var/lib/bluetooth/hid-devices`.
  CLI: `bthid connect ADDR | forget ADDR | list | selftest SECS`.
- `userland/bthid/src/report.rs`: HID report-descriptor parser + mapper (gamepad / keyboard / mouse roles, one evdev node
  each). Gamepads normalised like Linux hid-playstation/xpad (sticks X/Y + RX/RY -32768..32767, triggers Z/RZ 0..255,
  hat HAT0X/Y, BTN_SOUTH.. positional). Quirks: DS4 (0x01/0x11, GET_REPORT feature 0x02 to enable 0x11),
  DualSense (0x01/0x31, feature 0x05), Switch Pro and 8BitDo in Switch mode (057e:2009, 0x3F simple + 0x30 full),
  generic descriptor path for 8BitDo D-input (Start+B), Xbox One S classic-BT firmware, keyboards, mice.
- `/dev/uinput` (servers/evdev/src/uinput.rs, Linux ABI subset: SET_*BIT, ABS_SETUP, DEV_SETUP, DEV_CREATE/DESTROY,
  GET_VERSION/SYSNAME), root 0600; created nodes root:input 0660 like USB ones; kernel ioctl gate for type 'U'.
- MAME (`../mame` master 1bae6954a9b): 4 stable joystick devices "Gamepad 1..4", hot-plug scan of event2..15 every 1 s,
  axes/buttons/hat, default P1 directions from left stick + hat.

## Evidence
- Host tests `cargo test --manifest-path drivers/bthid-tests/Cargo.toml`: 9/9 (8BitDo D-input, DS4, DualSense, Switch Pro
  0x3F/0x30, keyboard+consumer, mouse, SDP reassembly + PnP, parser robustness). Descriptors are reconstructions (no
  real captures available); DS4/DualSense/Switch use the quirk path and ignore descriptors.
- `uinputtest` 23/23 (aarch64), added to the regress suite; evtest2 0 fails.
- QEMU aarch64: `bthid selftest 200 &` (synthetic DS4 0x11 reports through the real parser + uinput) then MAME:
  `Input: Adding joystick #1: Gamepad 1 ...` and `[mame] joystick 1: Wireless Controller (bthid selftest) (event2)`.

## Open
- Xbox Wireless (new firmware) is BLE HID-over-GATT: needs HOGP in btd (ATT/GATT client + SMP) - follow-up.
- Not tested on real radio/pads (needs the Pi 5 with btd). No rumble/LEDs; Switch Pro player LEDs keep blinking.
- uinput: no force-feedback, no legacy uinput_user_dev write setup.
- Desktop user must be in group `input` (gid 104) for MAME under COSMIC to open pad nodes (same as USB pads).
