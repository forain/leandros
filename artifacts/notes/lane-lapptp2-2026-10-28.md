# lane lapptp2 (2026-10-28): CPD0 Input Mode GET NAK

Design: artifacts/notes/design-lapptp2-2026-10-28.md. Change: drivers/src/i2c_hid.rs only.

## What changed
- 100 ms wait after RESET, read until the 0x0000 sentinel, drain pending reports.
- Drain after SET_REPORT and before each GET retry (5 x 10 ms).
- If GET Input Mode still NAKs, GET feature 8 (max contacts). Reads OK => firmware just lacks GET on Input Mode: accept unverified. Both NAK => PS/2 fallback.
- Confirmation in pad_poll: first id-3 report logs `touchpad: PTP reports confirmed`; a mouse report while unconfirmed resends SET_REPORT once.
- Plain input reads (old write+Sr+read form behind `i2c_hid.inreg=1`).
- `touchpad.debug=1` hex-dumps only the set-up transfers. Default stays `touchpad=ps2`.

## Laptop result (trial 9169e04c21e7, cmdline touchpad=i2c touchpad.debug=1)
Cause A (firmware): GET Input Mode `w=22 00 34 02 23 00` NAKs every time (abort 0x6000008, TXDATA_NOACK),
GET feature 8 `22 00 38 02 23 00` returns `04 00 08 05` OK (max contacts 5). Reset itself fine:
`reset done in 100 ms (sentinel true), drained 8` - the pad was already emitting id-3 PTP reports
(`20 00 03 01 6c 01 ...`) before our SET_REPORT. SET_REPORT `22 00 34 03 23 00 04 00 04 03` ACKed.
Driver went active (`touchpad: i2c active`), node /dev/input/event2 `CPD0 06CB:82F1 Touchpad`,
keyboard/other nodes present, cosmic-comp running, no transfer errors. No finger touch happened, so
`PTP reports confirmed` is still unseen. Laptop restored to good 2d3e00b630f2 (trial discarded, not promoted).

## USER finger test
Deploy: `remote.py deploy-kernel --reboot --cmdline 'touchpad=i2c touchpad.debug=1'` (or without debug).
1. Log in (leandro/leandro); do not touch the pad until the desktop is up.
2. One finger slow slide: pointer follows smoothly.
3. Two-finger scroll in a terminal or Firefox.
4. One-finger tap = left click; two-finger tap = right click.
5. Physical pad click = left click.
6. Palm on the pad edge while typing: no jumps.
7. Trackstick and its buttons still work.
8. Report which worked. Then check klog for `touchpad: PTP reports confirmed` or `pad still in mouse mode`.
If dead/erratic: use trackstick/keyboard, reboot; trial falls back to good.
