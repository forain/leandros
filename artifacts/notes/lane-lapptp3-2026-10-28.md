# lane lapptp3 (2026-10-28): review fixes for the opt-in I2C-HID touchpad

Fixes from review-lapptp2-2026-10-28 (findings 1-4, 6, 7, 10):
- psmouse (drivers/src/psmouse.rs): `set_touch_owner_i2c(bool)`; while set, the pad's own Synaptics packets are dropped and counted (`dropped_packets()`); W=3 trackstick pass-through is kept. i2c_hid sets it when active, clears it in `fallback()` and in the failed-setup path before PS2_REINIT.
- Runtime fallback only after >2 s of consecutive read errors with no good read (wall clock); one SET_POWER ON retry first.
- Fallback after 64 reports that are neither PTP nor mouse (counter reset by any PTP report).
- reset_wait and the GET retry loop have a 1 s monotonic deadline; real elapsed ms logged.
- get_feature checks echoed report id; oversize reports (len > wMaxInputLength) counted as errors and logged once; inreg knob cached; maxc-only success logged as "Input Mode unverified".
- `touchpad.debug=1` prints every 5 s: reports/frames/empty/errs/unknown and "ps2 pad packets dropped". `touchpad.fail=1` already existed (forces fallback 3 s after set-up).
- Not done: item 5 (GpioInt wait), 8 (log wording), 9 (hid_ptp standalone frames).

Tests: QEMU vfstest x86_64 + aarch64 pass (default cmdline). Laptop (INTEL=1 trial, one lock hold, restored to good 2d3e00b630f2):
- `touchpad=i2c touchpad.debug=1`: "touchpad: i2c active (CPD0)", stats lines with errs 0 for 5+ stat periods; /dev/input event0-3.
- `touchpad=i2c touchpad.fail=1`: "i2c failed (touchpad.fail=1), back to ps2", event2 removed, psmouse re-init ran (relative bare, as on a plain boot), no panic.

User finger test (still needed): boot `touchpad=i2c touchpad.debug=1`; slide a finger and confirm (1) the "ps2 pad packets dropped" counter rises while sliding (PS/2 stream quiesced, no double motion; if it stays 0 and motion doubles, the PS/2 side is not a Synaptics stream here - the boot log says "not Synaptics" so it may stay 0), (2) frames rises, (3) taps/scroll/trackstick/keyboard work.
