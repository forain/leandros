# Input

Every keyboard and pointer source in LeandrOS — virtio-input, USB HID, the x86 PS/2 controller and even serial-console bytes — funnels into one evdev server that exposes the Linux `input_event` interface on `/dev/input/event*`. Real libinput, libevdev and libxkbcommon run unmodified on top of it, with small shims standing in for the seatd and udevd daemons LeandrOS does not have. Delivery is lossless and per-open, and the one recorded "input loss under load" turned out to be the measuring apparatus.

## Overview

```
 virtio-keyboard ─┐                                   ┌─► /dev/input/event0  (keyboard, 13:64)
 virtio-tablet ───┤  virtio-input driver (polled)     │
 USB HID kbd/mouse┤  xHCI interrupt pipes (polled)    ├─► /dev/input/event1  (absolute pointer, 13:65)
 PS/2 keyboard ───┤  i8042 IRQ 1 (x86_64)  ──► evdev_server::push_event ──┤
 serial UART ─────┘  console tap (synthetic EV_KEY)   └─► in-kernel console line discipline
                                                          (VT chords, escape hatch, grabs)
 libinput 1.27.1 + libevdev + mtdev ── libudev shim (static device table) ── libseat shim (seat0, VT-aware)
 libxkbcommon 1.8.0 + xkeyboard-config 2.44 (/usr/share/X11/xkb, rules "evdev")
 cosmic-comp (smithay) ─► Wayland clients
```

| Piece | Code |
|---|---|
| evdev server | [`servers/evdev/src/lib.rs`](../servers/evdev/src/lib.rs) |
| virtio-input driver (keyboard and tablet) | [`drivers/src/virtio_keyboard.rs`](../drivers/src/virtio_keyboard.rs) |
| USB HID boot keyboard/mouse | [`drivers/usb/src/hid/mod.rs`](../drivers/usb/src/hid/mod.rs), glue in [`drivers/src/usb_hid.rs`](../drivers/src/usb_hid.rs) |
| PS/2 keyboard (x86_64) | [`arch/x86_64/src/keyboard.rs`](../arch/x86_64/src/keyboard.rs) |
| libseat / libudev shims, build recipes | [`ports/input-stack/`](../ports/input-stack) |

## The evdev server

### Devices and events

The server implements the `struct input_event { timeval, type, code, value }` layout from `linux/input.h` (24 bytes on both 64-bit architectures) and supports `EV_SYN`, `EV_KEY`, `EV_REL`, `EV_ABS` and `EV_LED`. Two nodes exist, registered as VFS character devices with real `st_rdev` numbers (up to four are possible):

- **`event0` — keyboard.** Full Linux scan-code mapping (`KEY_*`), so existing input libraries work without modification. Key down and up are delivered as separate events with no synthetic repeat; repeat is the application's job (USB typematic is the exception, below). Advertises `EV_LED` (Num/Caps/Scroll).
- **`event1` — absolute pointer.** QEMU's virtio-tablet, reporting `ABS_X`/`ABS_Y`, buttons, and `INPUT_PROP_POINTER` without `INPUT_PROP_DIRECT`, so libinput classifies it as a pointer rather than a touchscreen.

**virtio-keyboard** (`event0`) and **virtio-tablet** (`event1`) are attached by default on both architectures. Every virtio-input function on the bus is bound and routed to evdev, rather than only the first.

The ioctl surface is what libinput queries during device probe: `EVIOCGVERSION` (written through its pointer, as Linux does — it was once returned as the ioctl's value), `EVIOCGID`, `EVIOCGNAME`, `EVIOCGPHYS`/`UNIQ` (empty), `EVIOCGPROP`, `EVIOCGBIT` for every event type, `EVIOCGABS`, `EVIOCGKEY`/`LED`/`SW`, `EVIOCSCLOCKID`, `EVIOCGRAB` and `EVIOCREVOKE`, plus `FIONREAD`. Reads go through the standard `read` path and readiness through `poll`/`epoll`.

### Capabilities mirror the device

Capabilities are read from the device rather than hardcoded. The virtio-input driver reads the device's own `EV_BITS` bitmaps for `EV_REL` and `EV_KEY` from config space at probe and passes them to `set_device_caps()`; evdev mirrors the wheel axes (`REL_WHEEL`, `REL_HWHEEL` and their hi-res variants, never `REL_X/Y`) and the side/extra buttons.

That is how **mouse-wheel scrolling** was fixed (`249c185`). Wheel events always reached `event1`, but `EVIOCGBIT` advertised no `EV_REL`, and libevdev inside libinput drops every event whose type/code the node did not advertise — so libinput never built an axis event and nothing scrolled. Hardcoding the bits was rejected because libinput ≥ 1.19 switches to hi-res-only handling when `REL_WHEEL_HI_RES` is advertised; claiming it on a device that never sends it would break scrolling again. QEMU 11.1's tablet reports `REL_WHEEL` only, and libinput synthesises v120 from it (verified: 15 detents move cosmic-term 90 lines, its 6 lines per detent; Firefox scrolls both ways).

### Per-open queues

Linux gives every `open()` of an event node its own queue and broadcasts each event to all of them. LeandrOS once had a single ring per *device*, so two readers robbed each other: a census measured 128 keystrokes pushed, 112 taken by the in-kernel console drain and 16 delivered to the userspace client. That is cosmetic while one consumer matters and fatal once two do — which is exactly a greeter handing over to a session compositor. Now:

- Each open file description has a queue 256 events deep — enough for a whole fast drag (a tablet emits X, Y and SYN per motion frame) even for a reader waking at 60 Hz. Queues come from a pool of 16 shared across nodes, covering a compositor holding both nodes during a session handover.
- **Overflow is Linux's**: the whole queue is discarded and `SYN_DROPPED` queued, meaning "your state is stale, resync" — never half a motion frame.
- Each queue is tagged with the VT that was active when it was registered, and is skipped while another VT is on screen. A device opened while backgrounded therefore goes silent until its VT returns; registration logs the tag because the symptom is otherwise just "input stopped working".
- `EVIOCGRAB` gives exclusive delivery to the holder (`f3e133f`); verified with two readers and 30 injected moves: grabber 30, other 0. `EVIOCREVOKE` kills an open for good (`ENODEV` on read/ioctl/write, `POLLERR|POLLHUP`, grab released) as seatd expects; cosmic-comp closes its fds on seat disable instead, so revoke is exercised only by tests.

### Timestamps

Events are stamped from `arch_monotonic_ns()` — the clock `clock_gettime(CLOCK_MONOTONIC)` reads, at full resolution — before interrupts are masked or any lock is taken (`05bb0fe`). They used to come from the 100 Hz tick, so every event drained in one tick carried the same time. The first fix attempt was reverted because it "broke input entirely", and the cause was recorded as libinput rejecting the timestamps. **That was wrong**: libinput's absolute-pointer path never uses the timestamp, and its sanity checks are compiled out of the shipped library. The real cause was the aarch64 kernel's FP/SIMD clobber: that attempt inlined 128-bit division into IRQ context on a kernel built with NEON while the trap frame saved no vector state (see [aarch64](kernel/arch-aarch64.md)).

### Wake-ups

`push_event` used to wake the global poll channel — every polling task in the system — once per event, so a Shift press and release meant four whole-system wake storms. Wakes are now coalesced per burst and flushed once from the tick (`WAKE_MODE_BURST`, `7b62397`), about 8.75× fewer. This was built to fix a desktop input stall and did not — the stall had another cause entirely ([zink-and-virgl.md](graphics/zink-and-virgl.md#the-refuted-065-fps)) — but it is a real reduction and stayed.

### Routing to the console and VTs

`push_event` is the one choke point every keyboard source passes through, so VT policy lives there and covers new sources without being listed:

- **Ctrl+Alt+Fn** switches VTs; the chord is consumed for every queue at once, so no client sees the F-key that moved the display out from under it. The switch itself runs later in task context.
- **Ctrl+Alt+Esc** is the escape hatch back to VT 1 from a client that has both grabbed the keyboard and taken it out of `K_XLATE`. It sits upstream of the broadcast, which is the whole safety argument for enforcing `EVIOCGRAB` at all: this key is not routed, so it cannot be grabbed.
- A keystroke is copied to the kernel's line discipline only while the console owns the keyboard — not while a compositor holds the active VT in `KD_GRAPHICS` or the VT's owner reads raw scancodes. Otherwise a shell replays everything typed into a full-screen client once it exits (measured: ten injected keys came back as `aaaaaaaaaa` at the prompt).
- **Serial is exempt.** The serial line is the machine's primary console and is not a VT; the UART drain manufactures a synthetic `EV_KEY` with `value == 2` per byte on the keyboard node. Without the exemption the first compositor to take the display would cut off the only way to drive a headless boot.

Shift state tracking and printable-character mapping (including symbols) for the console are done in the tty layer; keys typed on text VTs 2–6 are translated with a US keymap into Linux `K_XLATE` bytes. See [TTY and VT](servers/tty-and-vt.md).

## Input sources

### virtio-input

One generic driver serves every virtio-input function. It is **polled from the 100 Hz timer tick** on CPU 0 with `try_lock` only — it runs in IRQ context, and blocking on a lock held by the interrupted task would wedge the CPU — which caps sampling at 100 Hz and adds up to 10 ms latency. Device-config bytes are read with plain `ldrb`/`strb` assembly accessors: once inlining changed, LLVM compiled a `read_volatile` as a write-back `ldrb w8, [x19, #2]!`, which traps with `ISV=0` and aborts QEMU under HVF (`Assertion isv failed`). Other drivers' `read_volatile` MMIO has the same latent exposure. The ring publication has the volatile accesses and barriers it once lacked — not a bug on x86_64's TSO, but nothing ordered those stores on aarch64.

### USB HID

Boot-protocol USB keyboards and mice work over xHCI interrupt pipes (`0fff571`), for real PCs and for `run-qemu.sh --usb-input` (which swaps virtio input for `usb-kbd`/`usb-mouse`):

- Each interrupt-IN pipe keeps eight one-packet transfers queued and is **harvested from the bootstrap CPU's tick**, next to the virtio-input poll. The controller lock is only `try_lock`ed, nothing in the tick path waits or allocates, and a halted endpoint is recovered by a small state machine spread across ticks (Reset Endpoint, Set TR Dequeue, `CLEAR_FEATURE(HALT)`), giving up after 16 recoveries.
- Keyboards: the 8-byte boot report is diffed against the previous one into Linux key edges using Linux's usbkbd usage table. **Typematic** re-sends the newest held key as another *press* after the console's `KDKBDREP` delay at its period (default 250/33 ms) — how PS/2 and QEMU's virtio keyboard deliver repeats; libinput ignores a press for a key already down, so COSMIC does not double-repeat. Value 2 is never used (it is the serial marker). LEDs follow the VT on screen, or the `EV_LED` events a compositor writes to `event0`.
- Mice feed `event1`. libinput rejects a node that is both absolute and relative, and the libudev shim knows only two nodes, so relative motion is integrated into an absolute position (one count = one framebuffer pixel, clamped, no acceleration) and the buttons and wheel are OR-ed into `event1`'s capabilities (`add_device_caps`).
- Only boot-protocol interfaces are used; report-protocol-only devices (QEMU's `usb-tablet`, media keys, game pads) are logged as unsupported. Devices behind a hub are not enumerated yet. See [PC hardware](hardware/pc.md).

### PS/2 keyboard (x86_64)

The i8042 driver decides a controller exists the way Linux does — the ACPI FADT `IAPC_BOOT_ARCH` 8042 bit, a status port that is not `0xFF`, a bounded flush, and a self-test with timeouts — and re-checks after the xHCI BIOS hand-off, because firmware often only *emulates* a PS/2 controller for USB keyboards until the OS takes the controller over. Before this, a real PC with no controller read floating ports as phantom keystrokes. There is no PS/2 mouse driver.

### Serial

UART input drains into the console tap's evdev queue. A full evdev queue is discarded Linux-style, which for a byte stream silently lost any burst over ~128 bytes arriving while the reader was busy. `serial_rx_drain` now keeps the tap below eight free slots, parks the excess in an ordered 4 KiB backlog, and stops reading the UART only when that is full — QEMU then back-pressures, since the 16550/PL011 accept only FIFO space; aarch64 masks the PL011 receive interrupts while throttled (`22c8e14`). Interrupt characters and Ctrl-T still act on arrival.

## Seat and device discovery

Real **libinput 1.27.1**, **libevdev 1.13.3**, **mtdev 1.1.6** and **libxkbcommon 1.8.0** with xkeyboard-config 2.44 are ported and run unmodified ([`ports/input-stack/NOTES.md`](../ports/input-stack/NOTES.md)). The two pieces of Linux plumbing beneath them that assume a daemon LeandrOS does not have are replaced by **shims**, built from tracked sources rather than shipped as blobs:

- **libudev** ([`shims/libudev`](../ports/input-stack/shims/libudev/libudev.c)) — a data-driven static device model: `card0` and `renderD128` (subsystem `drm`), `event0` (`ID_INPUT_KEYBOARD`) and `event1` (`ID_INPUT_MOUSE`), with syspaths under `/sys/class/{drm,input}` matching the kernel's synthetic sysfs. Enumeration honours subsystem, sysname and property filters; the monitor is one end of a socketpair that never delivers. It covers all 82 imports of `libudev-sys` and libinput's calls. libdrm does not use libudev — it reads sysfs directly — which is why the kernel also provides [synthetic sysfs](servers/vfs.md).
- **libseat** ([`shims/libseat`](../ports/input-stack/shims/libseat/libseat.c)) — one seat, `seat0`, with `open_device` a plain `open()` (the compositor runs with permission to open DRM and evdev nodes, and calls `SET_MASTER` itself). It is **VT-aware**: it opens `/dev/tty0`, confirms `VT_GETSTATE`, returns that fd from `get_fd()` so it becomes readable when the active VT changes, and fires `enable_seat`/`disable_seat` exactly on transitions of *its* VT — latched once at open — to and from the foreground. `switch_session` is `VT_ACTIVATE`, and the seat's VT is put in `KD_GRAPHICS` for the seat's lifetime. On a kernel without VTs it degrades to an always-active seat.

There is no seatd and no udevd. Measured across VT round trips, cosmic-comp drops and retakes DRM master itself through this shim, so no kernel-forced drop or `VT_PROCESS` handshake is needed.

## Delivery is lossless, and the instrument said otherwise

Input reaching Wayland clients was recorded for a long time as lossy under load — the evdev path starving at high pointer rates, with a rate ladder of measurements behind the claim (2.00, 1.12, 0.91, 0.85 events per injected move at 2, 10, 30, 60 moves/s). The ladder reproduced byte-for-byte, and **the cause was the measuring apparatus**.

`putc` polled the UART transmitter with no bound on either architecture, and QEMU's 16550 withholds `LSR.THRE` for exactly as long as its chardev back end refuses the byte. `putc` is reached from IRQ context — the periodic event census runs off the timer tick — so a serial consumer that stopped reading parked CPU 0 inside the timer handler: the tick froze and `poll_events()` never drained the event queue. Every lost event was a host-side `virtio_input_queue_full` (1572 of them, exactly the injected-minus-delivered count); nothing was lost inside the guest. The census that should have caught it could not: its poll counter and tick counter stopped together, so their ratio stayed 1:1.

Three 60 moves/s sweeps in one boot, differing only in who drained QEMU's serial chardev, settle it: **9.5% delivered with the consumer parked, 100% with it reading, 100% with nothing attached at all.** The harness caused what it measured, because it slept while holding the socket open. `putc` now waits against a cycle-counter deadline and latches a wedged flag so a back-pressured console costs one probe per byte (`af9f076`) — an iteration bound was not enough, because each emulated `in` is a host exit and 10,000 of them took ~100 ms, which moved the parked case only to 27%. Console output may be lost; an interrupt handler may not be stalled. After the fix the ladder reads 4.00 events per move at every rate (one move is `ABS_X, SYN, ABS_Y, SYN`), and a regression guard covers the console-stall shape. See [Methodology](development/methodology.md).

## Testing pointer input in QEMU

Inject with **QMP `input-send-event`** and absolute coordinates, never HMP `mouse_move`. `mouse_move` emits relative events, the only pointer is the absolute virtio-tablet, and QEMU's input core drops them with no diagnostic — 280 accepted `mouse_move` commands produced zero guest events, indistinguishable from a broken driver. HMP also cannot hold a chord (`sendkey` presses and releases in one shot), so Ctrl+Alt+Fn needs QMP too. The runner opens a permanent per-run QMP socket for this. Inside the guest, `evtest2` is the destination probe: `motion_abs_frame: PASS` proves motion arrived, and a button-only injection shows `motion_abs_frame: FAIL` with `motion_events > 0`, which is proof `EV_KEY` arrived.

A historical trap: `run-qemu.sh` has four QEMU command lines (UEFI and direct boot, per architecture), and a commit that said it added the tablet to "both architecture branches" put both hunks in the aarch64 half. x86_64 guests got q35's PS/2 mouse, for which there is no driver, while the automated harness — which built its own device list — stayed green for weeks.

## Limitations and open work

- On aarch64, clicks in COSMIC have been reported as ignored while the pointer moves; delivery to evdev is proven on both architectures, and the leading hypothesis is aim (host cursor versus guest mapping under host display scaling), which only a physical mouse can test.
- Horizontal wheel is mirrored but QEMU's tablet advertises none, so it is untested.
- USB devices behind hubs are not enumerated; USB mice have no acceleration.
- The text-VT keymap is US only; virtio keys have no console autorepeat.
- The libudev device table is static; hot-plug is not modelled.
- virtio-input is polled at 100 Hz rather than interrupt-driven.

## Code and tests

- [`servers/evdev/src/lib.rs`](../servers/evdev/src/lib.rs), [`drivers/src/virtio_keyboard.rs`](../drivers/src/virtio_keyboard.rs), [`drivers/usb/src/hid/mod.rs`](../drivers/usb/src/hid/mod.rs), [`drivers/src/usb_hid.rs`](../drivers/src/usb_hid.rs), [`arch/x86_64/src/keyboard.rs`](../arch/x86_64/src/keyboard.rs).
- [`userland/evtest2`](../userland/evtest2) — tablet capabilities (including the wheel), absolute motion frames, monotonic and sub-tick timestamps, wheel events, idle poll without false wakes.
- [`userland/evsplit`](../userland/evsplit) — two readers of one node: broadcast (60/60) and exclusive grab.
- [`userland/vttest`](../userland/vttest) — revoke, grab release, VT keyboard routing.

## See also

- [TTY and VT](servers/tty-and-vt.md) — line discipline, VT switching, keymaps.
- [Graphics](graphics/README.md) — the cursor plane and DRM master that pair with input.
- [Desktop](desktop/README.md) — how cosmic-comp consumes all of this.
- [Debugging](development/debugging.md) — serial capture pitfalls and QMP.
