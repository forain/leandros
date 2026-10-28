# Design: lane lapusbc — USB-C ports via the Alpine Ridge xHCI (3a:00.0) — 2026-10-27

Branch `lane/lapusbc` = main c5b3fd62, no commits (no code written). Worktree ../leandros-lapusbc.

## Root cause / findings (the premise is mostly REFUTED)

- The 3a:00.0 xHCI is **already a running bus**. Laptop klog (artifacts/remote-logs/20261027-224342-172.16.149.179/klog-current.txt:342-344):
  ```
  [XHCI] 3a:00.0 8086:15db BAR0 0xe5f00000 (64 KiB)
  [XHCI] bus 2: xHCI 1.10, 4 ports, 64 of 64 slots, 32-byte contexts, 64-bit DMA, 34 scratchpad pages
  [XHCI] 0 device(s), 1 mass-storage interface(s)      <- "1" is bus 1's stick, a running total
  ```
  `drivers/src/usb_hcd.rs:121 init()` probes every class 0c0330 function, so the TB xHCI is bus 2, and
  `usb-hotplug` (drivers/src/usb_storage.rs:1053, 250 ms) scans all of its root ports (drivers/usb/src/lib.rs:280).
- "No root hub in lsusb" (inventory note) is true for **every** bus: `lsusb` only lists enumerated devices
  (drivers/src/usb_hcd.rs:358 `device_count`/`device_info` → `usb::devices()`); there are no root-hub entries. So
  bus 2 with nothing plugged in is invisible to lsusb. That is cosmetic, not a dead controller.
- The controller stays powered at runtime: with the xHCI hot-removed, PORTSC reads 0xFFFFFFFF ⇒ CCS=1 and CSC=1
  on every scan ⇒ `scan_one` (lib.rs:224) logs "hot-plug enumeration failed" / "re-plugged" every 250 ms (CSC clears
  HOT_FAILED). The laptop klog has none of those lines for bus 2. Reason: the platform uses RTD3 (OS-controlled power);
  the controller only powers down when the OS puts the NHI/root port in D3 (_PS3), which we never do. Hence
  **no ICM driver-ready, no WMI/ACPI force-power (FPWR), no pciehp needed** for USB devices. (Linux: force_power
  via intel-wmi-thunderbolt is only for when the whole controller is absent; ICM in mode 3 tunnels PCIe itself.)
- USB-C routing on Alpine Ridge: the Type-C connector's SuperSpeed pairs go to the AR xHCI (bus 2, ports with
  protocol rev 3, up to 10G = PSIV 5 SPEED_SSP, already supported, context.rs:35); the USB 2.0 D+/D- pair is usually
  wired to the PCH xHCI (bus 1). So a USB 3 device in the USB-C port should appear on **bus 2**, a USB 2.0-only
  device on **bus 1**. TB3 devices/docks need ICM approval (out of scope).

## Changes (all small, inert on failure; knob `usb.tbxhci=0` disables D2)

D1. Boot log of the root hub (proof line for the laptop) — drivers/usb/src/xhci/mod.rs, in `Xhci::new` right after the
protocol walk (~line 222) or next to the "bus N: xHCI" log (~line 324): one line per bus
  `[XHCI] bus 2 root hub: USB2 ports 1-2, USB3 ports 3-4; PORTSC p1 0x2a0 p2 0x2a0 p3 0x2a0 p4 0x2a0`
  built from `port_rev` and `regs.portsc(p)` (group consecutive ports by rev; print raw PORTSC).

D2. Surprise-removal guard (pciehp-lite, for the case the TB firmware drops the link):
  - drivers/usb/src/xhci/mod.rs: add `pub fn controller_gone(hc: &Hc) -> bool { let x = hc.lock(); !x.dead && x.regs.op(OP_USBSTS) == 0xFFFF_FFFF }`.
  - drivers/usb/src/lib.rs:283 `hotplug_scan`: before the port loop, `if xhci::controller_gone(hc) { if !GONE_LOGGED[bus] { usb_log!("[USB] bus {} controller gone (reads all-ones); devices detached, scan paused\n", bus); detach every root port's devices; } continue; }`
    and when it reads sane again log `[USB] bus {} controller back` and re-probe is NOT attempted (BAR/rings lost):
    log only; a full re-probe (re-assign BAR from the saved value in usb_hcd CONTROLLERS, `usb::probe_xhci` as a new
    bus) is a follow-up only if the laptop ever shows the "gone" line.
  - Knob: `usb.tbxhci=0` (parse like `usb.early_handoff`, kernel/src/main.rs:1471) skips the guard.

D3. lsusb root hubs (Linux-compatible output) — optional, cheap:
  - drivers/usb/src/lib.rs: `pub fn controller_ports(bus) -> (u8 ports, bool has_usb3)` via `xhci::root_ports` + port_rev.
  - drivers/src/usb_hcd.rs:358 `device_count()` = controllers + devices; `device_info(i)` for `i < controller_count()`
    returns a synthetic entry: bus=i, address=1, vendor 0x1d6b, product 0x0003 (has USB3) / 0x0002, class 0x09,
    role ROLE_HUB|ROLE_ROOT_HUB (new const 0x40, also in userland/libc/src/devinfo.rs), port 0, speed SS/HS.
  - userland/lsusb/src/main.rs: role ROLE_ROOT_HUB prints "Linux Foundation 3.0 root hub" / "2.0 root hub"
    instead of the "hub (devices behind it are not supported yet)" text (main.rs:64/73).
  - Real device addresses stay as they are (slot ids; no clash with address 1? check: if a device has address 1, print
    the root hub as Device 001 anyway — Linux numbering differs; acceptable, or skip D3 if it breaks a regress test that
    counts lsusb lines: grep tests for `lsusb`).

## Test plan
- QEMU x86_64 (`-device qemu-xhci` already default): boot, expect the D1 line for bus 1 (`USB2 ports 1-4, USB3 ports 5-8`
  on qemu-xhci); `lsusb | head -2` shows `Bus 001 Device 001: ID 1d6b:0003 Linux Foundation 3.0 root hub`; usbtest/regress
  suites that use USB (`/bin/usbtest` if present, else default regress) green. aarch64 one boot + regress per protocol.
- D2 in QEMU: hard to inject; unit-check by `device_del` of a second xhci via QMP if the harness supports it, else
  review only (guard is a pure read of USBSTS).
- Laptop (lock.sh laptop, deploy-kernel trial, INTEL=1, then boot good back; do not promote):
  1. `dmesg | grep -E "bus 2 root hub|3a:00"` → `[XHCI] bus 2 root hub: USB2 ports 1-2, USB3 ports 3-4; PORTSC ...`
     (port split to be confirmed by the line itself).
  2. `lsusb` → `Bus 002 Device 001: ID 1d6b:0003 Linux Foundation 3.0 root hub`.
  3. After 10 min idle, `dmesg | grep "bus 2 controller gone"` → expect nothing (proves RTD3 premise).
  4. **Needs the user**: plug a USB 3 stick into the USB-C/TB3 port (native USB-C stick, or USB-A stick via a
     USB-C→A adapter). Expect within 1 s:
     `[USB] Bus 002 Device 00N: ID xxxx:xxxx ... [port 3|4, super-speed (5G)|super-speed+ (10G), class 08, bulk-only mass storage]`
     and a new /dev/sdX (`lsblk`). A USB 2.0-only device should appear on Bus 001 instead (that is correct routing).
     Unplug → `[USB-MSC] disk ... unplugged`.

## Not needed / refuted
ICM DRIVER_READY, WMI FPWR force-power, pciehp: not needed for USB-C USB devices on this platform (controller is
already up and stays up). TB3 device tunnelling/approval would need the ICM message path (separate lane).
