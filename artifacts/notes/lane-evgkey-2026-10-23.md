# lane/evgkey 2026-10-23 (L11.6 follow-up) -- FIXED in QEMU (aarch64)

evdev answered EVIOCGKEY/LED/SW with zeros, so after a SYN_DROPPED overflow libinput/libevdev resynced to
"nothing held": held buttons were synthetically released and presses in the dropped window were lost.

Fix (servers/evdev/src/lib.rs): per-device 96-byte KEY bitmap updated in `broadcast` on EV_KEY value 1/0
(value 2 ignored), returned by EVIOCGKEY (clamped to the user size). EVIOCGLED returns the LEDs a client last
wrote to the keyboard (CLIENT_LEDS); EVIOCGSW stays zero (no switches exist).

Guard: `evtest2 held` + `.claude/skills/run-leandros/evgkeytest.py` (in regress): QMP holds BTN_LEFT, then KEY_A,
checks the bit set/cleared, then holds BTN_LEFT across 300 motion frames with no reader and expects SYN_DROPPED
plus the bit still set. Red on main: held_btn_bit_set, held_key_bit_set, held_ovf_btn_still_held FAIL;
green with the fix. evbtntest and the full aarch64 regress pass.

Open: laptop EV_STATS confirmation (`drop` on event1 under a 1000 Hz mouse) is separate.
