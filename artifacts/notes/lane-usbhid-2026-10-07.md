# lane/usbhid — 2026-10-07: USB keyboards and mice, and the "phantom input"

Branch `lane/usbhid`, base `8177771` (lane usbinteg notes). Not pushed.

Trigger: the first boot of the USB-stick image on a real PC (x86_64, UEFI,
RTX 3070, no GPU driver). Root on the stick worked and getty printed
`login:`, but:

1. the keyboard did nothing (typing, Ctrl+Alt+F2, Caps Lock LED);
2. the console filled with digits after `login:`, e.g.
   `5135626502115136126602235136126602125136126602175149940`, then
   `120000320000132000033200001420000342000015200003520000…` and finally a
   repeating `1302000033020000`;
3. `[MMAP-BIG] pid=20 kind=file-lazy len_kib=… us=…` lines on the screen.

## 1. The digits were never input (root cause)

They are **kernel diagnostics with their labels stripped**.

A diagnostic line is assembled from pieces. Labels go out through either
`arch_serial_putc` (UART only, by design never mirrored to the
framebuffer/VT: `STAGE_DIRECT` in `kernel/src/main.rs`) or
`serial_print`/`serial_print_str` (UART + framebuffer). Numbers go through
`print_number`/`print_hex`, which **always** mirrored to the framebuffer. So
every line whose labels are UART-only reached the screen as bare digits, with
no separators and no newline (the `\n` is a label byte too). On QEMU nobody
looks at the framebuffer while these fire; on the real machine it was the
only console.

Decoding the photographed text against the two such printers that fire on a
normal boot:

* `sched::watchdog_scan` (`sched/src/lib.rs`):
  `"\n[WDOG] cpu" J " took no timer tick for ~" AGE " s (IRQs masked or timer dead): pid=" PID " not in a syscall preempt_disable=" P " local_ticks=" T " (seen from cpu" ME ")\n"`.
  Numbers only: `J AGE PID P T ME`. The report fires after 2 s, then every
  10 s, so AGE runs 2, 12, 22, 32, …:

      1 2 0 0 0 0 | 3 2 0 0 0 0 | 1 32 0 0 0 0 | 3 32 0 0 0 0 | 1 42 … | 1 102 0 0 0 0 | 3 102 0 0 0 0 | … | 1 302 0 0 0 0 | 3 302 0 0 0 0
      120000       320000        1320000         3320000        1420000  11020000          31020000                13020000          33020000

  i.e. `[WDOG] cpu1/cpu3 took no timer tick for ~N s … pid=0 … preempt_disable=0
  local_ticks=0 (seen from cpu0)` — every token in the user's transcript
  matches, including the final repeating `13020000 33020000` (~302 s, five
  minutes after boot). (The two `…2…` tokens of age 12/22 are where the
  transcript's line breaks fell.)
* `sched::clone::report_large_fork` (`sched/src/clone.rs`):
  `[FORK] tgid=T shared_pages=S private_rw_pages=P copied_pages=C clone_us=U`
  → `5 1356 2650 2 11 | 5 1361 2660 2 23 | 5 1361 2660 2 12 | 5 1361 2660 2 17 | 5 1499 40…`:
  tgid 5 is greetd (`graphical login started (greetd, pid 5)`), forking its
  greeter again and again because there is no GPU driver for the RTX 3070.

So the user's guess (`<1|3><N>20000`, EV_KEY/EV_ABS + autorepeat) had the
right shape but the wrong source: 1 and 3 are CPU numbers, N is the stall age
in seconds, and the zeros are pid / preempt count / local ticks / reporting
CPU. No byte of it went through the input path.

**Fix (`kernel/src/main.rs`):** a number now follows the route of the most
recent label on the same CPU (`LINE_DIRECT[cpu]`, set by `arch_serial_putc`
until its newline, cleared by any mirrored console print). Lines whose labels
are UART-only are now UART-only in full; lines whose labels are mirrored
(`[BLK] …`, `[EPOLL] …`, the boot banner) are unchanged.
`[MMAP-BIG]` is now UART-only as well (it is a per-mapping trace).

### What the watchdog was saying (real-hardware finding, NOT fixed here)

`cpu1`/`cpu3`: `local_ticks=0`, `pid=0` from boot on. Those per-CPU slots never
ran anything. `arch_cpu_id()` on x86_64 is the **raw LAPIC ID**, and the
per-CPU arrays (`MAX_CPUS = 8`) are indexed by it. On QEMU the IDs are dense
(0,1,2,3); on real hardware they are often sparse — e.g. a 4-core CPU without
SMT has LAPIC IDs 0,2,4,6. If three APs came up (`ACTIVE_CPUS` = 4), the
watchdog scans slots 0..3 and slots 1 and 3 belong to no CPU, which fits the
photo exactly (only cpu1 and cpu3, `local_ticks=0`, from the first report on).
This is an inference from the decoded text and the code, not observed: a
serial log or the CPU model of that machine would confirm it. Consequences
beyond the false watchdog:

* `smp_init` wakes **every** AP with a broadcast INIT-SIPI-SIPI and hands out
  stacks by arrival order from an array of `MAX_CPUS - 1` entries; on a machine
  with more than 8 hardware threads the extra APs index past that array
  (0x7F18 + 8·i inside the trampoline page) and start with a garbage or zero
  stack.
* A CPU whose LAPIC ID is ≥ 8 (any 8-core-with-SMT part, every hybrid Intel
  part, most AMD parts with more than one CCX) indexes `MAX_CPUS`-sized
  per-CPU arrays out of range or clamps onto another CPU's slot.
* `active_cpu_count()` counts started APs, not slot numbers, so code that
  loops `0..active_cpu_count()` misses real CPUs and visits empty slots.

Next step (separate lane): map LAPIC IDs to a dense CPU index (from the
Limine MP response or the MADT), start only the APs that fit, and store the
dense index in `IA32_TSC_AUX`.

## 2. Other ways real hardware could inject input (hardened)

Not what the photo showed, but each one would show up as phantom input on
some PC, so they are closed now:

* **i8042 (PS/2) driver** (`arch/x86_64/src/keyboard.rs`): it read port 0x60
  on every IRQ 1 without looking at the status register, with no presence
  check — mouse/touchpad (AUX) bytes became keystrokes and a floating bus
  would have become `0xFF` scancodes. Now:
  * ACPI FADT `IAPC_BOOT_ARCH` bit 1 (8042) clear on a revision ≥ 2 FADT →
    no controller, ports never touched (Linux's rule; parsed in
    `boot/src/acpi.rs`, `power::acpi_has_8042()`).
  * Else: status `0xFF` → absent; flush ≤ 16 stale bytes (a buffer that never
    empties → absent); READ COMMAND BYTE and self-test `0xAA → 0x55`, each
    with a timeout; restore the command byte with keyboard IRQ + translation
    on, AUX IRQ off; enable the keyboard port.
  * IRQ 1 is masked in the IOAPIC when there is no controller.
  * `recheck()` after `usb_hcd::init()`: the xHCI USBLEGSUP handoff ends the
    firmware's SMM PS/2 emulation; a controller that was only that emulation
    reads `0xFF` afterwards and is dropped.
  * The IRQ handler only trusts a byte with OBF set and AUX/parity/timeout
    clear, ignores status `0xFF` (and disables itself after 8 of those), and
    drops protocol bytes (00/FF overrun, FA ACK, FE resend, EE echo, FC/FD)
    and the Pause sequence.
* **COM1** (`arch/x86_64/src/lib.rs`): with no UART at 0x3F8 the LSR reads
  `0xFF`, whose data-ready bit fed an endless stream of `0xFF` bytes into the
  console. `0xFF` now means "no data".

## 3. USB HID boot keyboard + mouse

### xHCI core (additive): `drivers/usb/src/xhci/intr.rs`

An interrupt-IN pipe keeps `NTDS` = 8 one-packet TDs (Normal TRB, ISP|IOC,
length = wMaxPacketSize) queued on its endpoint, and is **harvested from the
BSP timer tick** (100 Hz, IRQ context), next to `virtio_keyboard::poll_events`
and before the tick's `flush_pending_wake`. The rules that keep it safe next
to block I/O (lane usbinteg's preempt-disabled spinning):

* the controller lock is only `try_lock`ed — when a task holds it (storage
  between two polls of its TD, boot enumeration) the tick is skipped and the
  reports wait in the controller for the next one; nothing in the tick path
  waits for anything;
* events are drained with `Xhci::drain_events()`, which is `poll_events()`
  without the `die()` (halting waits): a fatal controller error is left for
  the next task-context poll;
* a halted endpoint (STALL, three transaction errors) is recovered by a small
  state machine — Reset Endpoint submitted on one tick, collected on a later
  one, then Set TR Dequeue the same way, then CLEAR_FEATURE(HALT) as an
  asynchronous EP0 request after a STALL, then re-prime — and the pipe gives
  up after 16 recoveries;
* LED output reports are asynchronous EP0 requests (`set_report_nowait`); a
  failure disables LED updates for that device rather than waiting to recover
  EP0;
* nothing allocates in IRQ context: per-endpoint event vectors are allocated
  at their 64-entry cap and the command-completion list at 33, so filing an
  event never reallocates; reports come back in a fixed `Reports` array;
* report buffers are the endpoint's `bounce` `DmaBuf`, owned by the slot, so
  they follow the existing rule (freed only after Disable Slot).

Changes to `xhci/mod.rs` are limited to: `mod intr`, the `esit` field (Max
ESIT Payload, now written into endpoint contexts — 0 for bulk/control as
before), the `drain_events`/`poll_events` split, the two pre-sized vectors,
`Enumerated::hid`, and one branch in `enumerate_slot` (no BOT interface →
`hid::parse_config` → `intr::setup_hid`).

Configuration (task context, ordinary blocking control path): one Configure
Endpoint for every chosen interface (interrupt IN, interval from bInterval per
speed, HS mult bits as Max Burst, SS companion burst/bytes-per-interval),
SET_CONFIGURATION, then per interface SET_INTERFACE (alt ≠ 0),
SET_PROTOCOL(boot) (failure → interface skipped), SET_IDLE(0) (STALL ignored:
optional for mice), and for a keyboard SET_REPORT with the current LEDs.

### Class driver: `drivers/usb/src/hid/mod.rs`

* Interfaces: class 3, subclass 1, protocol 1 (keyboard) / 2 (mouse), with an
  interrupt IN endpoint. Only the **first** boot keyboard interface of a
  device is used (gaming keyboards expose several). Report-protocol-only HID
  interfaces (subclass 0: QEMU's usb-tablet, consumer-control/media-key
  interfaces, game pads) are flagged and logged as unsupported.
* Keyboard: 8-byte boot report diffed against the previous one into Linux key
  edges (modifier bits 0xE0..0xE7, then releases, then presses, then
  `SYN_REPORT`), with Linux's usbkbd usage→keycode table. ErrorRollOver
  reports leave the state alone.
* Typematic: the newest held non-lock key is re-sent as another **press**
  (value 1) after the console's KDKBDREP delay, at its period (default
  250/33 ms). That is how PS/2 typematic and QEMU's virtio keyboard deliver
  repeats, and the console on VT 1 has no repeat of its own; libinput ignores
  a press for a key already down, so COSMIC does not double-repeat. Value 2
  is never used (it is this tree's serial-byte marker on event0).
* Mouse: buttons (5), dx, dy, wheel (4th byte when present).
* Unplug: the pipe reports the device gone once; held keys and buttons are
  released.

### Glue: `drivers/src/usb_hid.rs`

Into the **same evdev nodes as virtio-input**: keys to event0 (so the
console getty, Ctrl+Alt+Fn — recognised inside `push_event` — and COSMIC need
no change), mouse to event1. event1 is an absolute pointer and libinput
rejects a node that is both absolute and relative, and the libudev shim only
knows event0/event1, so relative motion is integrated into an absolute
position (one count = one boot-framebuffer pixel, clamped); BTN_LEFT..EXTRA
and REL_WHEEL are OR-ed into event1's advertised caps
(`evdev_server::add_device_caps`). No pointer acceleration.

LEDs: the lock state of the VT on screen (`tty_server::keyboard::get_led`)
while the console owns the keyboard; while a compositor owns it, the EV_LED
events it writes to event0 (`evdev_server::client_leds`; event0 now advertises
EV_LED with Num/Caps/Scroll, EVIOCGLED already answered zeros). Sent whenever
they change.

### Logs and the post-getty summary

* USB log lines go to the console now (UART + framebuffer), not UART only:
  `[USB] Bus 001 Device 002: ID 0627:0001 QEMU QEMU USB Keyboard [port 6,
  high-speed (480M), class 03, HID keyboard]`. Hubs get an extra line saying
  devices behind them are not enumerated.
* `SYS_USBDEV_INFO` fills its 3 spare bytes with role/port/speed; `lsusb`
  prints them; `lsusb --summary` is run by init right before the first login
  prompt and again after the "NO GPU RENDERER" banner / last greeter respawn
  (both repaint the console over the first copy), followed by the `login: `
  prompt the repaint wiped (the waiting login process does not reprint it).
  Nothing is printed, and nothing forked, without USB devices, so default
  boots and their serial harnesses see exactly one prompt as before.

```
USB devices:                                        (x86_64, USB stick root)
  Bus 001 Device 001: ID 46f4:0001 port 1 super-speed - mass storage
  Bus 001 Device 002: ID 0627:0001 port 6 high-speed - keyboard
  Bus 001 Device 003: ID 0627:0001 port 7 high-speed - mouse

USB devices:                                        (x86_64, kbd + mouse + tablet)
  Bus 001 Device 001: ID 0627:0001 port 5 high-speed - keyboard
  Bus 001 Device 002: ID 0627:0001 port 6 high-speed - mouse
  Bus 001 Device 003: ID 0627:0001 port 7 high-speed - HID device (not a boot keyboard/mouse: unsupported)
```

### Test harness

* `run-qemu.sh <arch> --usb-input[=kbd,mouse,tablet] [--no-i8042]`
* `LEANDROS_USB_INPUT=1|kbd,mouse,tablet LEANDROS_NO_I8042=1 driver.py start <arch>`
  (combines with `LEANDROS_USB_STICK=<img>`; device ids usbkbd/usbmouse/usbtablet)
* key/mouse injection: QMP `input-send-event` (key down/up per character,
  `rel` x/y, `btn`), which QEMU routes to the active USB HID device.

## 4. Tests

Mac (aarch64 host): aarch64 under HVF, x86_64 under TCG; Homebrew QEMU, the
GPU QEMU for the virgl run. Full `./scripts/build-all.sh` (release, both
arches, stack-frame check OK: largest 34,960 B of 49,152). Input injected with
QMP `input-send-event`, never through serial, and with **no virtio-input
device** in any USB run.

| test | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| enumeration, `LEANDROS_USB_INPUT=1` (virtio disks, xHCI + usb-kbd + usb-mouse) | `[USB] … QEMU USB Keyboard [port 5, high-speed (480M), class 03, HID keyboard]`, `… Mouse … HID mouse`, summary above `login:` | same |
| log in at the console getty by typing `root⏎root⏎` on the USB keyboard, then `echo HID-OK-$((6*7))⏎` | `HID-OK-42` | `HID-OK-42` |
| Ctrl+Alt+F2 / F3 on the USB keyboard | `/proc/vtstat`: `active 2`, `switches 1`; tty2 login on screen | `active 3` |
| Caps Lock on a text VT | `vt2 … led=6`; `ab`+Caps+`cd` shows `ABcd` | `led=6`, `ABC` |
| Caps Lock LED reaches the device (QEMU `-trace usb_xhci_xfer_start`) | — | one EP0 transfer on the keyboard slot per Caps toggle (22 → 23 → 24) |
| typematic: key held 0.8 s / 1.2 s | `x` repeated ~17×, Backspace ~30× (250/33 ms) | — |
| USB-only keyboard: `q35,i8042=off` + usb-kbd/mouse/tablet | n/a | `[I8042] ACPI FADT: no PS/2 controller (IAPC_BOOT_ARCH 8042 clear), not probing`; login + `USBONLY-42`; tablet listed as unsupported HID, nothing else affected |
| COSMIC (virgl): greeter password typed on USB keyboard | session starts | (not run: TCG) |
| COSMIC: USB mouse moved 72,360 counts + left click on the dock | App Library opens; typing `term` filters to COSMIC Terminal | (not run) |
| USB stick root + USB keyboard/mouse (`LEANDROS_USB_STICK` + `LEANDROS_USB_INPUT=1`) | `/dev/root -> sda2`, login on the USB keyboard, `echo DURINGIO-42` typed while `dd` read 2.0 GB from sda (700 MB/s, 2.8 s) | same with `i8042=off`: 983 MB in 6.0 s while typing, `DURINGIO-42` |
| PS/2 still works (x86, q35 i8042, usb-mouse only: QEMU routes keys to PS/2) | n/a | `[I8042] PS/2 controller present (self-test passed)`; login, `PS2-OK-42`, Ctrl+Alt+F2 → `active 2` |
| no digit garbage: 6 `[FORK]` lines on serial during the x86 stick run | — | framebuffer screenshot shows only the session text |
| default boots (virtio, no xHCI): `runtests regress`, after the first and after the final build | **15/15 RC=0** twice; no `[USB]`/`[I8042]` line, no summary | **15/15 RC=0** twice; only new line `[I8042] PS/2 controller present (self-test passed)` |
| final build, x86 USB-only, no GPU: what the screen ends with | — | NO GPU banner, `USB devices:` (keyboard, mouse), `login: root`, `Password:`, shell, `FINAL-42` — typed on the USB keyboard |

Not reproducible in QEMU, so not tested: the original phantom stream itself
(QEMU's CPUs all tick); SMM USB legacy emulation and its disappearance at the
handoff (`recheck`); a real touchpad's AUX bytes.

## 5. Only real hardware can confirm

* The keyboard now works on that PC: needs the keyboard on a **root port**.
  On many desktops the front-panel/back ports are direct xHCI root ports
  (fine); a keyboard behind any hub — including a monitor hub or a
  keyboard with a built-in hub (common on gaming keyboards: the keyboard
  then *is* behind a hub) — is not enumerated yet: the summary will show a
  `hub (devices behind it are not supported yet)` line. Hub support is the
  parallel lane.
* Real HID devices: boot-protocol report sizes, SET_IDLE/SET_REPORT
  behaviour, keyboards with several interfaces (only the first boot keyboard
  interface is used), FS/LS devices' interval encoding on a real xHCI, Max
  ESIT payload acceptance on Intel/AMD controllers, transaction-error
  recovery (Reset Endpoint / Set TR Dequeue from the tick).
* Caps/Num LED on the physical keyboard (QEMU only shows the SET_REPORT
  transfer). Num Lock starts ON (the console's default), so the firmware's
  Num LED stays lit until toggled.
* The i8042 decisions on that board: FADT 8042 bit, self-test through SMM
  emulation, `[I8042] PS/2 controller gone after the xHCI BIOS handoff …` if
  it was emulation only.
* Whether the screen is clean: the `[WDOG]`/`[FORK]` digits are gone by
  construction, but the watchdog will keep reporting cpu1/cpu3 on serial until
  the LAPIC-ID indexing is fixed (section 1).
* Mouse feel: one count = one framebuffer pixel, no acceleration, on the
  absolute pointer node.

## 6. Files

* `kernel/src/main.rs` — numbers follow their line's route (`LINE_DIRECT`)
* `kernel/src/syscall.rs` — `[MMAP-BIG]` UART-only; `SYS_USBDEV_INFO` role/port/speed
* `arch/x86_64/src/keyboard.rs`, `ioapic.rs` (`mask_irq`), `lib.rs` (COM1 0xFF)
* `boot/src/acpi.rs`, `kernel/src/power.rs` — FADT revision + IAPC_BOOT_ARCH
* `kernel/src/init.rs` — i8042 recheck after the xHCI handoff
* `drivers/usb/src/xhci/intr.rs` (new), `xhci/mod.rs` (minimal hooks)
* `drivers/usb/src/hid/mod.rs` (new), `drivers/usb/src/lib.rs`
* `drivers/src/usb_hid.rs` (new), `usb_hcd.rs`, both arch `timer.rs`
* `servers/evdev/src/lib.rs` — `add_device_caps`, EV_LED caps + `client_leds`
* `userland/lsusb`, `userland/init`, `userland/libc/src/devinfo.rs`
* `scripts/run-qemu.sh`, `.claude/skills/run-leandros/driver.py`
