# Lane vt2master (openbugs #56): VT2 games refused on the laptop — NOT REPRODUCED in QEMU; instrumented + plan

## Verdict: PARTIAL (no QEMU repro; observable added; laptop measurement plan written)
- QEMU aarch64 (HVF, virgl), COSMIC logged in on VT1 through the greeter, then exactly the laptop's gv.sh path:
  root on the serial shell `vttest chvt 2`, `/bin/doom` started from that shell (no VT ctty), `chvt 1`, `chvt 2`.
  doomsession `DOOMSESSION_VT_VIA=chvt --assert-vt`: PASS {textvt-doom 1.0, again 1.0, desktop-vs-game 0.9997,
  desktop-back-vs-term 0.0388}. So neither `vttest chvt` nor DOOM lacking a VT ctty is the cause.
- QEMU switch_log: `1>2:ioctl:586 2>1:ioctl:590 1>2:ioctl:592` (only the harness's vttest pids); cosmic-comp drops
  master on the switch away (drop_ok 1->2, master_by_vt vt1=0 while VT2 shows) and re-takes it on return; DOOM gets
  VT2's slot implicitly. Laptop instead: cosmic-comp keeps its VT1 slot and the active VT returns to 1 unasked.
- Per-VT master slots mean a held VT1 grant cannot refuse a VT2 client; DOOM's SET_MODE EACCES and MAME's later
  FLIP_PAGE EACCES both require VT2 to be off screen at that moment, i.e. the bounce is the bug. Kernel switch
  sources (code read, complete): VT_ACTIVATE, keyboard chord/keymap, Ctrl+Alt+Esc rescue, VT_PROCESS watchdog;
  userspace VT_ACTIVATE callers: vttest, kparks, libseat shim switch_session (cosmic-comp), greetd.

## Changes
- servers/tty/src/vt.rs: `/proc/vtstat` `switch_log tick:from>to:src:tgid` — last 8 completed switches with their
  origin (ioctl + caller tgid, key, rescue, watchdog). Measurement only.
- doomsession.py: `DOOMSESSION_VT_VIA=chvt` (gv.sh path) and a new assert "text VT stayed on screen while the game
  ran" (vtstat `active` during the game), which is the laptop failure's first symptom.
- vttest suite aarch64: 1/1 PASS.

## Open
- Laptop run per the plan (SP design/vt2master.md): switch_log names the bouncer; libseat shim trace
  (LEANDROS_INPUT_TRACE) shows whether cosmic-comp ever saw disable_seat on i915.
