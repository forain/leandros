# Terminals: the tty server, PTYs and virtual terminals

[`servers/tty`](../../servers/tty) is an in-kernel library that owns everything termios-shaped: the console's per-process termios and its line-discipline signals, terminal job control, the pseudo-terminal pool behind `/dev/ptmx` and `/dev/pts/N`, the six virtual terminals with their keyboard translation, and — for historical reasons — the POSIX timer tables. This page covers the terminal half; POSIX timers are described in [../kernel/time.md](../kernel/time.md).

## Overview

| Module | File | Summary |
|---|---|---|
| console termios | [`lib.rs`](../../servers/tty/src/lib.rs) | one `Termios` per process for fd 0/1/2 on the console, which never appear in any fd table |
| job control | [`jobctl.rs`](../../servers/tty/src/jobctl.rs) | Linux's `__tty_check_change` and `n_tty_read` job-control gate, shared by the console and every pty |
| PTYs | [`pty.rs`](../../servers/tty/src/pty.rs) | 64 pairs with a full termios line discipline |
| VTs | [`vt.rs`](../../servers/tty/src/vt.rs) | `/dev/tty1..6`, per-VT text mirrors, switching, VT/KD ioctls, `KD_GRAPHICS` ownership |
| keyboard | [`keyboard.rs`](../../servers/tty/src/keyboard.rs), [`defkeymap.rs`](../../servers/tty/src/defkeymap.rs) | a port of Linux's `keyboard.c` for the text consoles, with Linux's default keymap |

## The console

fd 0, 1 and 2 of a process on the console are served by the kernel's read/write fast paths, so `tcgetattr`/`tcsetattr`/`isatty` on them are answered from the tty server's per-process `CONSOLE_TERMIOS` table (sized to the 512-process limit). Bytes from the UART drain pass through `console_intercept_byte`, the ISIG half of the line discipline, so `^C`, `^Z` and `^\` from the serial console or the keyboard become real `SIGINT`/`SIGTSTP`/`SIGQUIT` for the foreground process group.

The console has two sinks — the UART and the framebuffer's own VT emulator — and **the framebuffer is authoritative**: two terminals of different geometry cannot both be right off one byte stream. So a cursor-position query (`CSI 6 n`) is answered from the framebuffer's cursor and never reaches the UART, and `TIOCGWINSZ` reports the framebuffer's real cell grid (1920×1080 at 8×16 is 240×67) rather than 80×24. Synthetic console input such as that CPR reply must be visible to `poll`, not just to `read`: crossterm waits for the reply in epoll, and a reply invisible to readiness left brush waiting out reedline's 2 s timeout and then leaked the stale reply into the next command (`bin/[13;1Recho`). See [../graphics/console.md](../graphics/console.md) for the emulator itself.

Console writes are atomic per call, and **kernel diagnostic lines are atomic per line**: each CPU stages its diagnostic bytes into a line and finished lines go through an 8 KiB FIFO outbox, written only by whoever holds the console output lock (taken with `try_lock`, never spun on from interrupt context). Before this, kernel lines built piece by piece were woven character by character into `Password:` and the shell prompt during the burst of messages when the compositor starts (`[FOPRasKs]wo rtdg: id=4` is "Password: " inside "[FORK] tgid=4"). That was recorded as "serial login loses input about one boot in ten"; no input byte was ever lost.

## Job control

Job control is POSIX-shaped and backed by a real `Stopped` task state in the scheduler (see [../kernel/scheduler.md](../kernel/scheduler.md) and [../kernel/signals.md](../kernel/signals.md)), so `^Z`, `fg` and `bg` in brush behave as on Linux.

- **One rule for every terminal.** `jobctl::check` is applied before a read, before a write when `TOSTOP` is set, and before `tcsetpgrp`, by the console and by every pty alike. A terminal exerts job control only over processes for which it is the controlling terminal — the same session — so a daemon whose stdio happens to be the console is never stopped by it. The foreground group proceeds; a background group gets `SIGTTIN` (read) or `SIGTTOU` (write, ioctl) sent to the *whole group*, unless the signal is ignored or blocked (a read then fails `EIO`, a write proceeds) or the group is orphaned (everything is `EIO`, since nothing could ever continue it).
- **No syscall restart, so the stop runs in place.** Linux returns `-ERESTARTSYS`; this kernel has no restart mechanism, so `check` stops the caller right there and reports `Retry`, and the caller re-evaluates from the top once continued — exactly what a restarted syscall would do. A process that catches the signal gets `EINTR`, as on Linux without `SA_RESTART`. A pending `SIGKILL` breaks out instead of re-raising, or a background job could never be killed.
- **Session-scoped `tcsetpgrp`** (`TIOCSPGRP`/`TIOCGPGRP`), `TIOCSCTTY`, `TIOCNOTTY`, `getsid`, and `setpgid` with Linux's checks (see [security.md](security.md)).
- **Orphaned groups**: when a process group becomes orphaned and has stopped members, they receive `SIGHUP` then `SIGCONT`.
- The line discipline generates `SIGINT`, `SIGQUIT`, `SIGTSTP` (`VSUSP`) and `SIGWINCH` on `TIOCSWINSZ`.

Job control was verified interactively on both architectures, including a `^Z` / `fg` / `^C` round trip over the real serial console (brush reported `[1]+ Stopped`, resumed, and exited 130). `#!` scripts are executed by the kernel (interpreter depth 4, then `ELOOP`), which job-control tests and session launch scripts rely on.

## Pseudo-terminals

### How it works

A PTY is two rings plus a line discipline, and the discipline is termios, which this crate owns — so the pool lives here and the VFS holds only the vnode. `VnodeKind::Pty { pair, is_master }` is deliberately shaped like `VnodeKind::Pipe`, so a pty end inherits every fd behaviour a pipe already had (dup2, fork, `O_NONBLOCK`, epoll edges, SCM_RIGHTS).

- **Rings named for who reads them**: `to_slave` (what the user typed; input processing, `ISIG`, echo) and `to_master` (what the program printed; `OPOST`/`ONLCR` plus injected echo). 4 KiB in, 16 KiB out.
- **Canonical mode commits whole lines.** In `ICANON` the master's bytes accumulate in `canon` and only a terminator or `VEOF` moves the line into `to_slave`. So `to_slave.count > 0` *means* "a complete line is readable" — the POSIX condition — and a canonical reader can never be woken for a half-typed line.
- 64 pairs (`MAX_PTYS`; `/dev/pts` names stay two digits). Every cosmic-term window and tab holds one; eight capped a desktop at about seven terminals. `ptmx` returns `ENOSPC` when full, as Linux does.
- Each pty records its controlling session, and `/dev/tty` resolves through `pty::ctty_for_sid` to the caller's controlling pty, or to the console when there is none.

### The contract cosmic-term enforces

cosmic-term reaches the kernel through rustix's `linux_raw` backend — raw `syscall` instructions that bypass musl — so these are kernel behaviours, not libc conventions:

- **`TIOCGPTPEER` must not answer `ENOTTY`**, the conventionally correct reply to an unknown ioctl. rustix's `openpt` tries it first and falls back to `TIOCGPTN` + `open("/dev/pts/N")` only on `ENOSYS` or `EPERM`; anything else makes every terminal tab fail to open. It is implemented in the VFS (`pty_get_peer`), because it allocates a descriptor and descriptors are the VFS's business.
- **A master `read()` after the last slave closes returns `EIO`, not EOF.** alacritty_terminal (inside cosmic-term) registers the master *level-triggered* and swallows `EIO` on purpose, waiting for `SIGCHLD`. An earlier draft returned EOF because it seemed friendlier; for this consumer it is an unbreakable 100 % CPU spin, since 0 is not an error it stops for and the readiness never clears.
- Closing the last *master* sends `SIGHUP` to the slave's foreground group **and** makes slave reads return EOF. Linux returns `EIO` there; EOF is returned as well as the signal because a shell exits reliably on EOF, while an `EIO` a shell mishandles becomes a spin in the read retry loop.
- Panics to avoid: cosmic-term asserts the passwd uid matches, asserts `fcntl(F_SETFL)` returns 0, and exits on a failed `TIOCSCTTY` or `TIOCSWINSZ`.

A related bug class: an orphaned cursor-position reply. crossterm returns any `CursorPosition` already in its private queue with no request/response correlation, so one stray reply desynchronises `position()` permanently, and `tcflush` cannot reach a queue inside the process. Output in cosmic-term was invisible on the slow accelerator until crossterm was fixed (a fork wired through brush's `[patch.crates-io]`; `cargo tree -i crossterm` must show one node). The pty bug that had been stranding the reply on the fast target was fixed in the same change — it could not land alone, because it was what had been hiding the crossterm bug.

## Virtual terminals

### What a VT is here

The framebuffer console and the DRM scanout are the same surface, so a VT answers "which console owns the display right now" for N = 6. Each VT has a character-cell **mirror** (`Screen`) so a switch can put its text back: the framebuffer's `putc` remains the only renderer, and `vt.rs` runs a deliberate transcription of the same escape-sequence rules over the same byte stream to remember the result. The two must move together; the scroll step is read back from the framebuffer at run time rather than duplicated, because a mismatched step would desynchronise the cursor and land every later repaint on the wrong row. All the mirrors (~865 KB) are zero images in `.bss` — the default foreground colour is stored XOR-inverted and the keyboard mode biased by one so that "default" is zero.

- **VT 1 is the console.** Kernel output and the serial getty's console session draw into VT 1's plane, and VT 1 is where the graphical session lives. Kernel lines appear on serial and VT 1 only — the documented Linux configuration `console=ttyS0 console=tty1`. Linux with no `console=` would print to the foreground VT, but kernel lines here carry no log level and what the kernel prints after boot is debug tracing that distributions keep off VTs; revisit once lines have levels. A panic brings VT 1 forward.
- **VT 2..6 are real terminals.** `/dev/ttyN` is the slave of a pty pair whose master is the kernel. Keys typed on a text VT other than 1 are translated through the keymap into a per-VT queue and pumped through the line discipline; they never reach the console tap. Output is drawn into that VT's plane and is on screen only when it is active. `init` runs a login on each (setsid, `TIOCSCTTY`, respawn with a quick-fail cap). The VT answers `CSI 6n`/`5n`/`c` like the Linux console. The last close of a session resets its session and termios; `VT_DISALLOCATE` refuses an open session with `EBUSY`. `fstat` and `readlink` report `/dev/ttyN`, so `ttyname` works.
- **Ioctls**: `VT_OPENQRY`, `VT_GETSTATE`, `VT_ACTIVATE` (asynchronous, as on Linux), `VT_WAITACTIVE` (parks on the poll wait channel rather than spinning), `VT_SETMODE`/`VT_RELDISP` (`VT_PROCESS` release handshake with a 5 s watchdog), `VT_DISALLOCATE`, `KDSETMODE`/`KDGETMODE`, `KDGKBMODE`/`KDSKBMODE`, and the keyboard ioctls below. `/proc/vtstat` exposes counters.
- **`/dev/tty0` is a pollable "active VT changed" edge.** `poll` reports `POLLIN` while the active VT differs from what this open last saw; `read` consumes the edge and returns one byte, the new VT number; with no edge pending, `EAGAIN`. The state is per open (two openers are independent), and the epoll edge sequence is the active VT number itself. This is a LeandrOS-specific channel: Linux signals VT changes through `VT_PROCESS`, which is implemented too, but a signal cannot be waited on with a file descriptor and the libseat shim's `get_fd()`/`dispatch()` contract needs one.

### Switching

- **Ctrl+Alt+F1..F6** is recognised in the input IRQ (`chord_key`) and queued as a single atomic store; the switch itself runs later in task context. It is **gated on a text keyboard mode** (`K_XLATE` or `K_UNICODE`), as on Linux: in `K_RAW`/`K_MEDIUMRAW` the owner does its own switching. Modifier state moves only on real press/release edges — the UART drain pushes every serial byte as a key event with value 2, and counting those as presses once latched Ctrl+Alt after an ordinary serial command, turning a later plain Esc into the rescue chord.
- **Ctrl+Alt+Esc is the escape hatch** and carries **no keyboard-mode gate**. A client that sets `K_RAW` *and* grabs the keyboard with `EVIOCGRAB` has closed every other keyboard route back, and that client is trivial to write by accident. The hatch releases every evdev grab immediately and, in task context, forces VT 1 back to `KD_TEXT` and `VT_AUTO`, abandons any handshake in flight, and moves the display there. Only VT 1 is touched, so a healthy compositor on another VT is not knocked out of graphics mode. The chord is swallowed from all clients, so a COSMIC binding on it would never fire.
- **`KD_GRAPHICS` has an owner.** `KDSETMODE(KD_GRAPHICS)` records the caller's thread group; when that owner dies, the VT returns to `KD_TEXT` (and is repainted if on screen). On Linux the session manager does this; here the seat manager lives inside the compositor.

### Device arbitration

A switch must take the display *and* the input devices away from the session that is no longer in front.

- **DRM master is derived, not notified.** A DRM open is master iff it holds master *and* the VT it was granted on is the active one (`MASTER_OPEN == me && MASTER_VT == vt::active()`). A switch writes nothing into the DRM layer, so it cannot race a present — the race the previous scanout-revoke approach kept losing. Master is re-checked on the way *out* of a present as well as the way in, because an in-flight present could otherwise gate the console back off after the switch; it is re-armed automatically on return; and an open that dropped master is not re-granted by its next present (Linux behaviour). Details in [../graphics/drm-kms.md](../graphics/drm-kms.md).
- **evdev is VT-gated** and `SYN_DROPPED` is forced onto every queue pinned to a VT when it becomes active, so a client resynchronises instead of replaying stale state. The gate exempts the whole *frame*, not just the key event: exempting only `EV_KEY` would hand a reader keystrokes libinput can never complete. `EVIOCGRAB` is enforced (measured exclusive: the grabber saw 30 of 30 injected moves, another reader 0), and `EVIOCREVOKE` is implemented with Linux semantics. See [../input.md](../input.md).
- **The libseat shim** ([`ports/input-stack/shims/libseat/libseat.c`](../../ports/input-stack/shims/libseat/libseat.c)) is both consumer and producer: it emits `disable_seat`/`enable_seat` from the `/dev/tty0` edge, so cosmic-comp closes its evdev fds on the way out and reopens them on return, and drops and re-takes DRM master itself. At `open_seat` it takes the owned VT from `XDG_VTNR` (set to 1 in greetd's profile), else a VT controlling terminal, else **the foreground VT at `open_seat` time** read through `VT_GETSTATE` — the same derivation DRM master uses, so the two agree by construction. It opens that VT, sets `KD_GRAPHICS` (only if it was `KD_TEXT`; restored in `close_seat`), and implements `switch_session` as `VT_ACTIVATE`. It deliberately does **not** set `K_OFF`, because the kernel's chord is disabled in non-text modes and that would trap the user in the session.

### Keyboard

[`keyboard.rs`](../../servers/tty/src/keyboard.rs) is Linux's `keyboard.c` for the text consoles: 16 keymaps (the Shift/AltGr/Ctrl/Alt combinations) defaulting to Linux's `defkeymap` (generated by `ports/kbd-keymaps/gen-defkeymap.py` from `loadkeys --mktable`), every keysym type, F-key strings, cursor and keypad keys tracking DECCKM/DECKPAM/LNM from VT output, modifiers with held-key counts, Caps/Num/Scroll lock, dead keys, Alt+keypad, and `Console_N`/`Incr`/`Decr`/`Last`.

- The default keyboard mode is **`K_UNICODE`** (Linux `default_utf8=1`).
- Ioctls: `KDGKBENT`/`KDSKBENT`, `KDGKBSENT`/`KDSKBSENT`, `KDKBDREP`, `KDGKBLED`/`KDSKBLED`, `KDGKBMETA`/`KDSKBMETA`, `KDGETLED`/`KDSETLED`. The write ioctls are root-only (Linux also allows the VT's controlling user).
- Autorepeat on VT 2..6 is 250 ms delay / 33 ms period, adjustable with `KDKBDREP`, run from evdev's end-of-tick drain; evdev clients do their own repeat, as on Linux.
- `/bin/loadkmap` ([`userland/loadkmap`](../../userland/loadkmap)) loads kbd's `loadkeys -b` / busybox binary keymaps through `KDSKBENT`. Keymaps ship in `/usr/share/keymaps` (us, uk, de-latin1 with and without dead keys, fr-latin1, es, br-abnt2), and `init` applies `/etc/vconsole.conf`'s `KEYMAP=` before the first getty.

### Refuted premises

- **"A grab structurally cannot block Ctrl+Alt+Fn"** was the premise grab enforcement was first filed on, and it was false: true of routing, irrelevant because the chord gates itself on the keyboard mode, so grab + `K_RAW` closes it upstream of routing. Enforcing on the original reasoning would have built the unrecoverable machine the work existed to prevent; the escape hatch is the consequence.
- **The libseat shim's silence was not a name test.** `ttyname_r` failed with `ENOTTY` before any name comparison (musl calls `isatty` first), and neither exporting `XDG_VTNR` nor inventing a controlling VT was right on its own: the one console session genuinely owns no VT — it *follows* the foreground VT. Starting COSMIC from VT 2 latched `owned_vt: 2`, which a hard-coded `XDG_VTNR=1` would have got wrong in the worst way (deactivated on the VT it was actually on).
- **A kernel-forced master drop or a `VT_PROCESS` handshake in the shim is not needed.** Measured: cosmic-comp drops and re-takes master itself on every switch (11 drops / 13 sets over five rounds on both architectures); one to three in-flight presents per switch are refused `EACCES` and the compositor is unaffected.
- **`EVIOCREVOKE` is not exercised by this stack** (cosmic-comp closes its evdev fds instead); it exists for Linux/seatd semantics and is tested by `vttest`.

### Verified behaviour

A COSMIC session switched to VT 2, logged in and typed there, and switched back, five times per architecture: no key leaks between VTs, VT 1 repainted zero times per return (the session holds `KD_GRAPHICS`), and the desktop came back pixel-identical (0.0 % difference). On a fast accelerator the first frame after return can show the stale VT 2 frame for 55–150 ms, as Linux does.

## Limitations and open work

- The one VT 1 repaint per boot at the greeter-to-session handoff remains: the greeter's compositor dies, so VT 1 briefly returns to text (Linux shows the same gap unless the display manager keeps the VT in graphics).
- A present already inside its handler can land pixels after the console repaint — self-healing, but a visible flash.
- VT 1 has no autorepeat (its keys come from the console tap, not the IRQ translation); Enter on VT 1 stays `\n` because the console has no `ICRNL` stage.
- Keyboard LED state is reported but not written to the device.
- No PAM session, no utmpx records for VT logins ([security.md](security.md#open-gaps)).
- `setsid()` still succeeds for a process-group leader (Linux: `EPERM`).
- The console's default `c_cc` indices look off (`cc[11]` = `^Q` lands in `VEOL`).
- A staged kernel line with no trailing newline waits until its CPU prints one; interrupt-context lines can still be dropped under host back-pressure, by design.

## Code and tests

- [`servers/tty/src/lib.rs`](../../servers/tty/src/lib.rs), [`jobctl.rs`](../../servers/tty/src/jobctl.rs), [`pty.rs`](../../servers/tty/src/pty.rs), [`vt.rs`](../../servers/tty/src/vt.rs), [`keyboard.rs`](../../servers/tty/src/keyboard.rs).
- [`servers/vfs/src/lib.rs`](../../servers/vfs/src/lib.rs) — `VnodeKind::Pty`/`DevVt`, `pty_get_peer`, `fd_vt_number`.
- [`kernel/src/main.rs`](../../kernel/src/main.rs) — console output, the per-CPU diagnostic outbox, `kernel_vt_fb_write`.
- [`userland/init`](../../userland/init) — gettys on serial and `tty2..6`, `loadkmap --vconsole`.
- Tests: `userland/ptytest`, `userland/vttest` (VT ioctls, sessions, revoke, keyboard), `userland/jobtest`, `userland/exectest` (`#!` scripts), `userland/evsplit` (grab exclusivity), `userland/sigtest2` (stop/continue).

## See also

- [../graphics/console.md](../graphics/console.md) — the framebuffer VT emulator and scanout arbitration.
- [../graphics/drm-kms.md](../graphics/drm-kms.md) — per-VT DRM master.
- [../input.md](../input.md) — evdev, the VT gate, grabs, the libseat and libudev shims.
- [../kernel/signals.md](../kernel/signals.md) — job-control stops and delivery.
- [security.md](security.md) — logins on the gettys.
