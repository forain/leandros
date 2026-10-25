# Lane mamekbd — 2026-10-25 (openbugs L12 #65)

**FIXED.** MAME inside COSMIC hard-reset the emulated machine every ~10 s and never exited at `-str`.

## Root cause
`/dev/input/event0` also carries every serial-console byte as `EV_KEY` with `value == 2` and the ASCII
byte as the code, never followed by a release (servers/evdev `push_serial_byte`, deliberate: the serial
console must reach any event0 reader). MAME's evdev reader (`../mame src/osd/leandros/leandros_input.cpp`)
took any `value != 0` as a press. The session harness polls the guest every 10 s over serial with
`cat /proc/[0-9]*/cmdline ...; __r=$?; ...`: `*` = 42 = KEY_LEFTSHIFT (held forever), then `=` = 61 =
KEY_F3 -> Shift+F3 = MAME hard reset, once per serial poll.
Evidence (LEANDROS_MAME_KEYLOG=1, red run): 2210 decoded evdev events, all value=2, decoding to the
harness's serial command text; every `code=61` is followed within 6 lines by a new
"drawing in a Wayland window"; 14 "Average speed" lines / 17 windows for one `-str 30` run.

The uncommitted edit left in ../mame (input_event time fields as two int64) was correct (the kernel
struct is Linux's 24-byte layout; the old u64+u32 layout read type from the usec padding) — committed.

## Fix (../mame, local master, not pushed)
- `beefdddebfd` evdev `input_event` layout matches the kernel's.
- `22fedcad5cc` evdev: only value 0/1 edges change state. Wayland: bind `wl_seat`, `get_keyboard`, use
  `wl_keyboard` enter/leave/key (evdev codes) while the window has focus; evdev opened only without
  `WAYLAND_DISPLAY` (or when no Wayland window opened). `LEANDROS_MAME_KEYLOG=1` logs decoded keys.

## Guard
`mamesession.py` now runs with the keyboard ON by default and requires `mame-single-run`
(exactly one "Average speed" line, one window, mame exited) and `mame-wl-key` (QEMU `sendkey 5` arrives
as `key src=wayland code=6 value=1`).
- red (pre-fix, aarch64): runs=14 windows=17, rc=1.
- green (post-fix, aarch64): runs=1 windows=1, speed 100%, wl-key pass, rc=0.
- bare console (serial `mame captcomm -str 20`): one run, exits (100%, 19 s).

## Open
- Bare-VT evdev path after the fix was not exercised with live serial noise during the run (logic is the
  value filter only). x86_64 not run (per-lane rule: aarch64 only).
