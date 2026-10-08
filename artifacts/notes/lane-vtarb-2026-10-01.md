# lane vtarb — 2026-10-02 (VT device arbitration; TODO items 14/20)

Outcome: **FIXED** (with two premises REFUTED by measurement, below).
Branch `lane/vtarb` @ `3aef4c9` (base origin/main `6703408`). Evidence: `lane-vtarb-files/`.

## State found on main (verified before writing code)
- DRM master already existed (`959710d`/item 14): per-open master, EACCES on the 7 present ioctls
  while the holder's VT is off screen, auto-rearm on return. EVIOCGRAB enforced; evdev VT gate on.
- Missing: EVIOCREVOKE (accepted+ignored); exclusive grab *delivery* never measured; and **VT 2..6
  had no session at all** — `/dev/ttyN` was the console, console output followed the active VT, so
  VT2 showed black (integ1001/gpufencefb) and keys typed there fed the serial shell.

## What changed
- `servers/tty/src/vt.rs`, `pty.rs`: VT 2..6 are real terminals: `/dev/ttyN` = slave of a pty pair
  whose master is the kernel. Keys typed on a text VT≠1 are translated (US keymap, Linux K_XLATE
  bytes: Enter=CR, BS=DEL, arrows/Home/End/PgUp/Del CSI, Ctrl/Alt/Caps) into a per-VT IRQ queue,
  pumped through the ldisc by the reader/poller/syscall-return; never reach the console tap.
  Output drawn into that VT's plane, on screen only when active (DRAW_LOCK vs complete_switch).
  Console (VT1) now mirrors to VT1 only and draws only while VT1 is up. VT answers CSI 6n/5n/c like
  the Linux console (brush/crossterm needs CPR). Last close of a VT session resets sid/termios.
  VT_DISALLOCATE refuses an open session (EBUSY). `/proc/vtstat` counters.
- `servers/vfs`: open `/dev/tty2..6` -> session; fstat ino/readlink = `/dev/ttyN` (ttyname works).
- `kernel/src/syscall.rs`: VT/KD ioctls on a session fd go to that VT. `kernel/src/main.rs`:
  `kernel_vt_fb_write` seam, console draw gated to VT1. `drivers/framebuffer.rs`: panic brings VT1 forward.
- `servers/evdev`: **EVIOCREVOKE** (arg must be 0 else EINVAL; then read/ioctl/write ENODEV, poll
  POLLERR|POLLHUP, grab released, other opens untouched). EVIOCGVERSION now writes through its
  pointer (was returned as the ioctl value). Keyboard routing hook for text VTs.
- `drivers/drm_device_interface.rs`: an open that held and dropped master is not re-granted
  implicitly by its next present (EACCES, as Linux); master counters.
- `userland/init`: login on tty2..tty6 (setsid, TIOCSCTTY, respawn, quick-fail cap; sweep/mem-guard
  spare VT sessions). `userland/login`: no self-echo on a pty (username printed twice otherwise).
- Tests: vttest 21 -> 24 (`session_node`, `revoke`, `revoke_releases_grab`); `evsplit <n> 1 grab`.

## Evidence
| | aarch64 Mac HVF | x86_64 desktop KVM |
|---|---|---|
| vttest | 24/24 | 24/24 |
| drmsmoke / ptytest / polltest | RC=0 / 0 / 0 | 0 / 0 / **1** (poll_linux_semantics, see open) |
| evsplit grab, 30 injected moves | grabber 30, other 0 (EXCLUSIVE); control 30/30 | same |
| headless VT2 keyboard login, `tty` -> /dev/tty2 | OK, serial shell saw nothing | OK |
| greeter: F2 (login prompt) -> F1 | back diff 0.0083 % | 0.0 % (virgl), 0.0 % (Venus) |
| COSMIC session ×5: VT2 login+typing, VT1 redraw+typing, cross-capture cats | 5/5, 0 leaks, back diff 0.0 % | virgl 5/5 (diff ≤0.0004 %), Venus 3/3 |
| exceptions/PF/SEGV/panic/WDOG | 0 | 0 |

## Measured, and REFUTED premises
- **cosmic-comp (smithay via our libseat shim) drops and re-takes DRM master itself**: per switch away
  `drop_ok` +1, per return `set_ok` +1 (11 drops / 13 sets over 5 rounds, both arches); 1-3 in-flight
  presents refused EACCES per switch, compositor unaffected. A kernel-side forced drop or a
  VT_PROCESS/VT_RELDISP handshake in the shim is **not needed** — the shim stays passive; the VT-scoped
  gate covers the window between the switch and smithay's pause.
- **EVIOCREVOKE is not exercised by our stack** (`revokes=0` in every session): cosmic-comp closes its
  evdev fds on disable_seat. Implemented for Linux/seatd semantics, tested by vttest only. Kernel VT
  gate did filter 26 events on x86_64 (`vt_filtered`) that arrived before cosmic-comp closed its fds.

## Open
- x86_64 polltest `poll_linux_semantics` FAIL is environmental: the desktop's `../relibc` is
  `1746b13a`, without the relibcpoll fix (`fd1967e1`, local-only on the Mac). Not this lane.
- On return to VT1 the console text of VT1 flashes until cosmic-comp's first present (VT1 is not
  KD_GRAPHICS because the shim never sets it). Pre-existing, unchanged.
- Kernel log lines no longer appear on VT2..6 (they go to VT1's plane), as with `console=tty1`.
- Keymap is US only; no autorepeat for virtio keys (value 2 is the serial marker, pre-existing).
- `default_console` termios c_cc indices look off (cc[11]=^Q lands in VEOL) — pre-existing, noted only.
