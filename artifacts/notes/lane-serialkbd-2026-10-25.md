# lane serialkbd, 2026-10-25: FIXED (openbugs L12 #67)

**Bug.** `servers/evdev` `push_serial_byte` broadcast every serial-console byte to every
`/dev/input/event0` reader as `EV_KEY code=<ASCII byte> value=2`, never released. Evdev readers
(libinput, MAME on a VT, DOOM's SDL shim) saw raw ASCII as Linux keycodes: '*' (42) = held
KEY_LEFTSHIFT, '=' (61) = KEY_F3, etc.

**Why it existed.** The console tty pops its input from an in-kernel evdev client
(`CONSOLE_OPEN_ID`) on the keyboard node, so the UART drain rides that queue. Broadcasting to
userspace readers was a side effect, defended in a comment for "a userspace event0 reader on a
serial-only machine". Nothing in the harness relied on it: doomsession/mamesession/evgkey type through
QMP `sendkey`/`input-send-event`; pollnarrow's UART case reads fd 0, not event0.

**Fix (Linux model: the serial console is a tty).**
- Serial bytes go to the console queue only (`push_event_ex(.., users=false)`); no userspace event0
  open sees them.
- Opt-in: `evdev.serialkbd=1` on the kernel cmdline, or root's `SERIALKBD_IOCTL` (0x45f0, arg 1/0) on
  `/dev/input/hotplug`, adds a separate hot-plugged keyboard node "serial-kbd" (BUS_VIRTUAL
  0627:0003). Each byte becomes a US-layout key (`evdev_server::ascii_to_key`): Ctrl/Shift pressed
  around it when needed, press and release, each in its own SYN frame. event0 never gets them.
- DOOM's `leandros_sdl.c` no longer decodes value 2 as ASCII (it is autorepeat of a real keycode).

**Guard.** `host:serialkbdtest.py` (in regress), guest side `/bin/evtest2 serial <ms> [on]`. Types
`#Ab*=1\r` on serial while watching event0 (and serial-kbd when opted in).
- Red on the unfixed kernel (aarch64): `event0_keys=7`, opt-in ioctl ENOTTY, 5 fails.
- Green: `event0_keys=0` in both modes; opt-in presses=10 releases=10 repeats=0 held=0, codes
  `+42 +4 -4 -42 +42 +30 -30 -42 +48 -48 +42 +9 -9 -42 +13 -13 +2 -2`.

**Note.** Testing needed the lane/bootfail driver.py patch (openbugs #68, harness reported "boot
failed" though the guest reached `login:`), applied locally only, not committed here.

**Open.** The cmdline knob path calls the same `set_serial_kbd` as the ioctl, but was not booted with
`evdev.serialkbd=1` in this lane.
