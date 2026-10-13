# lane/pointer — touchpad, pointing stick, scrolling (2026-10-12)

Target: HP EliteBook 840 G5 (103c:83b2, i5-8350U, Sunrise Point-LP).
Branched from `lane/laptop` 644a642. Not pushed, not merged.

## What was built

| Commit | What |
|---|---|
| 63f6024 | `drivers/src/rel_pointer.rs`: one relative-pointer path to `event1` (motion integrated into ABS_X/Y, per-source buttons, v120 wheels → `REL_WHEEL_HI_RES` + accumulated `REL_WHEEL`). USB mice (`usb_hid.rs`) now use it. |
| c43a937 | `arch/x86_64/src/psmouse.rs` (AUX probe/setup, polled, task context) + `drivers/src/psmouse.rs` (packet decoding, IRQ context). ImPS/2 / Explorer / bare PS/2; Synaptics absolute W mode with tap, clickpad areas, two-finger scroll, pass-through stick with middle-button scroll. ACPI id scan. AUX clock hygiene. |

Design points:
- No new evdev node: libinput rejects abs+rel on one node and the libudev
  shim knows only event0/event1, so touchpad AND stick feed `event1` like USB
  mice. Pointer gestures are done in the kernel (no libinput touchpad code).
- IRQ 12 is routed to vector 33 (the keyboard's). `keyboard::on_irq` drains
  up to 16 bytes per IRQ and splits by the status AUX bit — AUX bytes are
  always read (to `psmouse::on_byte`, which drops them if no protocol).
- `REL_WHEEL_HI_RES` is advertised only when Synaptics absolute mode is up
  (libinput then ignores legacy `REL_WHEEL`; every `rel_pointer` source sends
  both, virtio-tablet sends legacy only — they do not coexist on a laptop).
- Probe order: AUX clock on, A9 test, reset (FF), Synaptics identify; if
  Synaptics + extended caps + sane ranges → mode 0xC5 (abs|80pps|no-gesture|W),
  verify via modes query, enable guest (sliced F4 + F3 0x28); else reset and
  IMPS/EXPS knocks. Any failure → relative; total failure → AUX clock off.

## Verified (QEMU, release build)

- x86_64, `LEANDROS_USB_INPUT=kbd` (q35 PS/2 mouse is the only pointer):
  `[PSMOUSE] ready: IntelliMouse Explorer`; HMP `mouse_move` → raw packets →
  evtest2 `motion_abs_frame: PASS` (12 events); QMP wheel → `wheel_up=2`,
  `wheel_frame: PASS`.
- x86_64 default (virtio kbd+tablet + PS/2): boots, wheel PASS.
- aarch64 with `LEANDROS_USB_INPUT=kbd,mouse`: USB mouse via the new shared
  path, wheel PASS + motion PASS (step 3: USB wheel path confirmed, not
  rewritten; lane/wheel's EV_REL caps still advertised).
- NOT verifiable in QEMU: Synaptics absolute mode, tap, two-finger scroll,
  pass-through stick. QEMU's mouse answers the Synaptics identify with
  `00 00 64` (not Synaptics) — the fallback path is what ran.

## Boot-5 report (coordinator) and what changed for it

Boot 5 on `lane/laptop` (no AUX driver): touching the pad made the cursor
jump/disappear, then the session froze. The old keyboard probe left the AUX
*clock* enabled with the AUX IRQ off, so touchpad bytes could sit unread in
the shared 8042 output buffer and block keystrokes behind them. Now:
(a) the keyboard probe disables the AUX clock (CTR bit 0x20 + 0xA7) until
psmouse is up, and psmouse disables it again on failure / `psmouse.proto=off`;
(b) the shared handler never leaves an AUX byte unread;
(c) conservative motion: touchpad full stroke = one screen width, ramp ≤ 2x;
relative-mode Synaptics gets 4 counts/mm. The cursor "moving fast" on boot 5
was not from our code (no AUX driver existed) — possibly firmware/SMM PS/2
emulation or USB legacy; if it recurs with `psmouse.proto=off`, look there.

## HANDOFF — laptop boot

Boot with `psmouse.debug=1` the first time. Grep the log for `[PSMOUSE]`:

1. `ACPI ... "SYN...."/"ELAN...."/EisaId(PNP0C50)/EisaId(PNP0F13) in _HID/_CID of Device XXXX`
   — PNP0C50 under an I2C controller ⇒ I2C-HID touchpad (this driver will
   not see it; needs an I2C-HID driver). PNP0F13 + SYN ids ⇒ PS/2 Synaptics.
2. `i8042 command byte 0x.., AUX test 0x00 (ok)` — AUX port exists.
3. `reset: BAT 0xaa id 0x00` — a device answered on AUX. If instead
   `no PS/2 pointer: no reply to AUX reset` ⇒ the pad is not on PS/2.
4. `synaptics identify: xx 47 yy -> Synaptics vN.M` then `capabilities`,
   `caps: extended=1 ... passthrough=1 multifinger=1`, `ext: ... clickpad=1
   intertouch(SMBus)=1`, `range x A..B y C..D`, `modes now: 3b 47 c5`.
5. `pass-through device enable: sent` — the stick should then move.
6. `ready: Synaptics absolute (...)` or `ready: IntelliMouse ...`.
7. With debug: `pkt N: b0 b1 b2 b3 b4 b5` (first 200, then 1 in 25),
   `tap N`, `lost sync at byte ...` (a few is fine, a stream is not).

What to try if it misbehaves (cmdline knobs):
- Stick dead / jumpy pad in absolute mode: `psmouse.proto=imps` (Synaptics
  relative mode; the pad merges the stick's packets, wheel via the pad's own
  scroll zone if any, no two-finger scroll).
- Anything wedges or keyboard dies: `psmouse.proto=off` (AUX untouched,
  clock off — the pre-lane behaviour minus the AUX-clock hazard).
- Speed: `psmouse.speed=<pct>` (touchpad, default 100),
  `psmouse.stickspeed=<pct>` (default 100); `psmouse.tap=0` disables taps.
- `psmouse.proto=exps|bare|synaptics` force a protocol.

## Risks / open

- Synaptics code is written from the Linux/Synaptics spec, untested on
  hardware. Specific gaps: no AGM / image-sensor mode (two-finger detection
  relies on W=0/1 with `multifinger`); extended-button packets
  (`multi_btn_no>0`, `(b0^b3)&2`) are not masked out of X/Y — if the log
  shows that, add Linux's masking; no tap-and-drag; no palm rejection beyond
  "no tap when W>=12".
- `intertouch(SMBus)=1` is likely on this pad; PS/2 still works (Linux's
  `psmouse.synaptics_intertouch=0` path), RMI4-over-SMBus is not implemented.
- The 3 stick buttons may come either in pass-through packets (handled) or
  as extended buttons in pad packets (not handled — see above).
- Keyboard bytes arriving during the ~1 s AUX probe at boot are dropped.
- IRQ 12 is routed without consulting MADT interrupt source overrides
  (edge/high, the ISA default; same assumption as IRQ 1).

## HANDOFF lane/vtkbd (2026-10-12)

**Boot 6 VT2 garbage (`root` -> `^]^R^]^O...`) = Ctrl+Alt stuck, not an escape bug.**
The bytes were `ESC ^R` per letter (the `^]` was almost certainly `^[` read off
the screen): ctrl_alt_map[KEY_R] = 0x0812 = Meta_Control_r -> `1b 12`. Cause:
PS/2 keyboards repeat in HARDWARE; holding Ctrl+Alt ~0.5 s before F2 sends
several Ctrl/Alt makes, the i8042 driver forwards each as value 1, and
`servers/tty/src/keyboard.rs::process` counted each as a press (`down[]` += 1)
while the single break only took one off. QEMU `sendkey` never repeats, so it
never showed. Not COSMIC-specific (VT 1's console reader had the same flaw).
Fix: a make for an already-down key is treated as a repeat (Linux input-core
semantics); plus `keyboard::resync_irq()` rebuilds modifiers from the held-key
bitmap in `vt::complete_switch`. Verified x86_64 QEMU: QMP 3x ctrl down, 3x alt
down, f2, releases, then `sendkey r o o t` -> tty2 shows `login: root`.

**Touchpad identify:** now tried 3 ways (E6-sliced, Linux exact E8x4+E9, sliced
after a fresh reset), each logged as `synaptics identify#N`. `00 46 12` matches
no known vendor magic; one bit off 0x47 with major=2, so watch boot 7 for
whether attempts #1/#2 read 0x47. `psmouse.proto=synaptics` now proceeds past a
bad identify/caps magic (opt-in). ImPS/2 knock ID is logged and retried once.

Boot-7 knobs: none needed for the keyboard. Touchpad: read the `identify#0..2`
lines; if all show 0x46, try `psmouse.proto=synaptics`, and fall back to
`psmouse.proto=bare` if the pointer goes wrong.
