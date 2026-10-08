# lane vtpolish — 2026-10-02 (vtarb follow-ups: VT1 flash, VT keyboard, kernel log)

Outcome: **FIXED** (items 1, 2). Item 3 was a decision: keep the current routing, with the reasons below.
Branch `lane/vtpolish` @ `ec40e9f` (base origin/main `dce4140`). Evidence: `lane-vtpolish-files/`.

## 1. VT1 text flash on return to the session
- Found: `KDSETMODE`/`KDGETMODE` were already in the tty server. The cause was that nothing ever set
  `KD_GRAPHICS` (the libseat shim never did). Each return to VT1 went through the text path in
  `complete_switch`: revoke the scanout and repaint VT1's text plane until the compositor presented again.
- libseat shim (`ports/input-stack/shims/libseat/libseat.c`): at `open_seat` it opens `/dev/tty<own VT>`
  and sets `KD_GRAPHICS`, as seatd/logind do. It only takes ownership if the VT was in `KD_TEXT`, and it
  restores `KD_TEXT` in `close_seat`. `switch_session` is now `VT_ACTIVATE`. `K_OFF` is NOT set: the
  kernel's Ctrl+Alt+Fn is disabled in non-text keyboard modes, so setting it would trap the user in
  the session.
- Kernel (`servers/tty/src/vt.rs`): `KD_GRAPHICS` records its owner tgid. `cleanup_pid` puts the VT back
  in `KD_TEXT` (and repaints it if it is on screen) when the owner dies. On Linux the session manager
  does this; here the seat manager lives inside the compositor.
- `ports/greetd/data/{profile,greeter-env}`: `XDG_VTNR=1` (pam_systemd's variable). Without it the shim
  binds to the VT that is on screen when the compositor starts. Measured: a session that came up while
  VT2 was on screen took VT2 into `KD_GRAPHICS`.
- New counters in `/proc/vtstat`: per-VT `repaints`, `gfx_owner`, `led`; `kd text/graphics/reverted`;
  `kbd_repeat`.

| COSMIC session ↔ VT2 ×5 (vtpsess.py) | aarch64 Mac HVF virgl | x86_64 desktop KVM virgl |
|---|---|---|
| VT1 repaints per return | 0,0,0,0,0 | 0,0,0,0,0 |
| back-to-back VNC grabs (~2.5 s) classed "other" (console text/black) | 0 of ~150 | 0 of ~100 |
| first frame after return | stale VT2 frame for 55–150 ms, then the session (Linux shows the same) | session already at the first grab (60 ms) |
| settled diff vs before | 0.0 % | 0.0 % (r0 1.9 %, clock) |
| kd / owner | graphics, owner = session cosmic-comp | same |

The one VT1 repaint left per boot is at the greeter→session handoff: the greeter's compositor dies, so
VT1 goes back to text. This is unchanged from before. Linux shows the same gap unless the display
manager keeps the VT in graphics.

Regression, vtarb's `vtsess.py` (VT2 login + typing, VT1 typing, key isolation ×5): aarch64 5/5 ok and
x86_64 5/5 ok, `serial_cmd_leak=0`, back diff 0.0 %, and 0 exc/PF/SEGV/panic/WDOG.

## 2. VT keyboard
- `servers/tty/src/keyboard.rs` is Linux's `keyboard.c` for the text consoles:
  - keymaps (16 maps covering the Shift/AltGr/Ctrl/Alt combinations); the default is Linux's
    `defkeymap`, generated into `defkeymap.rs` by `ports/kbd-keymaps/gen-defkeymap.py` from
    `loadkeys --mktable`
  - every keysym type: Latin-1/Unicode (UTF-8 in K_UNICODE), F-key strings, cursor keys and keypad
    with DECCKM/DECKPAM/LNM tracked from VT output
  - modifiers with held-key counts and `compute_shiftstate`; Caps/Num/Scroll lock flags
  - dead keys with the default accent table, Alt+keypad, Console_N/Incr/Decr/Last
- Ioctls: `KDGKBENT/KDSKBENT`, `KDGKBSENT/KDSKBSENT`, `KDKBDREP`, `KDGKBLED/KDSKBLED`,
  `KDGKBMETA/KDSKBMETA`; `KDGETLED/KDSETLED` report the lock state. The write ioctls are root-only;
  Linux also allows the VT's controlling user.
- The default keyboard mode is now **K_UNICODE** (Linux `default_utf8=1`). `K_XLATE` and `K_UNICODE`
  both count as text modes for the chord and console gates.
- VT1's console reader (`read_input_byte`) uses the same keymap and processes edges in queue order.
  Its modifier state is resynced from the IRQ-side state whenever VT1 regains the keyboard. Enter on
  VT1 stays `\n`, because the console has no ICRNL stage.
- Autorepeat: 250 ms delay / 33 ms period, changeable with `KDKBDREP`. It runs from evdev's
  end-of-tick drain, for text VTs 2..6 only; evdev clients do their own repeat, as on Linux.
- Keymap loading uses the smallest Linux-compatible approach: `/bin/loadkmap` reads kbd's
  `loadkeys -b` / busybox binary keymaps and issues `KDSKBENT`, deallocating keymaps the file does
  not define. Keymaps are shipped in `/usr/share/keymaps` (us, uk, de-latin1(-nodeadkeys), fr-latin1,
  es, br-abnt2; `de` = de-latin1, see the README). init runs `loadkmap --vconsole`, which applies
  `/etc/vconsole.conf KEYMAP=`, before the gettys start.

| VT2, keys via QEMU (vtpsuite.py) | aarch64 HVF | x86_64 KVM |
|---|---|---|
| vttest | **31/31** | **31/31** |
| ptytest / drmsmoke / polltest | RC 0/0/0 | 0/0/0 |
| hold `a` 2000 ms | 53 repeats (= (2000−250)/33), 54 a | 53 repeats |
| KDKBDREP 500/100, hold 2000 ms | 16 a (1 + 15) | same |
| CapsLock `ab1`, off `ab`, Shift-a | `41 42 31 61 62 41 0a`; led 6 → 2 | same |
| de-latin1: ä ß Ä ö ü z AltGr+q ´+e | `c3a4 c39f c384 c3b6 c3bc 7a 40 c3a9 0a` exact | same |
| vconsole.conf + reboot | `loadkmap: de-latin1: 10 keymaps x 128 keys`; ä ß typed | same |

## 3. Kernel log on VT2..6 — decision: keep it (`console=ttyS0 console=tty1` semantics)
With no `console=`, Linux prints to the foreground VT (tty0) and skips `KD_GRAPHICS`. That would not
disturb COSMIC. Here, though:
- kernel lines have no log level; and
- what the kernel prints after boot is debug tracing. A 13-min COSMIC + VT2 run printed 24 lines:
  20 `[FORK]` and 4 `[DRM]`. Each `[FORK]` is printed when *anyone* forks a large process (the serial
  shell included).

Linux prints such lines at DEBUG/INFO, below what any distro (`quiet` → loglevel 4) lets onto a VT.
Kernel output therefore keeps going to serial plus VT1's plane, the documented Linux configuration
`console=ttyS0 console=tty1`, and a panic still brings VT1 forward (`panic_front`). Revisit once
kernel lines carry a level.

## Notes / open
- **Desktop disk.** The desktop's samsung970pro512 was full (54 MB free) and the x86_64 build there
  failed with ENOSPC in the shared `bottom-leandros` sibling. I built x86_64 on the Mac and copied the
  three images to the desktop worktree to run under KVM. Session outputs went to /home (the samsung
  drive refilled mid-run). Everything is removed now.
- **VT1 autorepeat.** VT1 (console) has no autorepeat: its keys are read from the console tap queue,
  not translated in the IRQ. VT1 does have the keymap.
- **No LED to device.** The virtio-input status queue is not written; the lock state is only
  reported.
- **Harness parse bug.** vtpsuite.py's `hexof()` regex failed to parse; the bytes above are from the
  serial logs (`a64-suite2/serial-*.log`, `x64-suite/*`).
- **Shared files touched:** `kernel/src/syscall.rs` (read_input_byte, VT ioctl arg length),
  `servers/evdev/src/lib.rs` (`kbd_event` now/tick hook), `scripts/mkfs-f2fs-populated.py`,
  `userland/init`, `ports/greetd/data/{profile,greeter-env}`, libseat shim.
