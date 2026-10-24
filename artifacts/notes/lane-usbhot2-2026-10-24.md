# lane/usbhot2 — 2026-10-24 (L8.2 remainder)

Status: FIXED for (1) fast re-plug and (2) hubs. PARTIAL for (3) HID: unplug and re-plug work and other input is not wedged, but there are no per-device evdev nodes.

## What changed (drivers/usb, drivers/src/usb_storage.rs)
- Fast re-plug: the hot-plug scan takes the root-port PORTSC.CSC latch and the hub C_PORT_CONNECTION bit. If a known device's port changed since the last scan, the device is torn down and the port enumerated again, even though CCS still reads 1. The 250 ms poll is unchanged. Port Status Change Event TRBs are still not used: the latch alone closes the gap, and latency stays at most 250 ms.
- Hubs (USB 2.0):
  - Setup: the hub slot context gets Hub=1, the port count and the TT think time (Configure Endpoint with only A0; a failure is logged and not fatal). Ports are powered, then polled with GET_STATUS by the scan. The hub interrupt endpoint is not used.
  - Children: each one is reset through the hub, then Enable Slot and Address Device run with the route string. A full/low-speed device behind a high-speed hub gets the TT hub slot and port.
  - Boot: devices behind hubs are found at boot too, including nested hubs in the same pass. SuperSpeed hubs are refused, and so are hubs deeper than 5 tiers.
- Unplug:
  - Devices are keyed by (bus, slot); `XhciBulk::location` and `HotEvent::Gone` now carry the slot.
  - A departed device and everything behind it get `Slot.gone` set, and `port_connected` honours it, so transfers fail with Disconnected at once even when the root port still reads connected.
  - Mass storage goes to usb_storage as before, which then calls `usb::release_slot`.
  - HID pipes are detached (`hid::detach`); keys still held are released by the next poll. Their slots are then freed, and the hub leaves the scan.
- HID re-plug on the same port is now enumerated. Before, its stale DEVICES entry blocked it forever.

## Guard: usbunplugtest.py phases fastreplug | hub | hid (plus unplug, notready; `all`)
- fastreplug: device_del A and device_add C on the same port over one QMP connection, 0.0 ms apart.
  - Fixed: PASS. C appears as /dev/sda, A is gone and B is intact (3.0 s). The log shows "disconnected from port 1".
  - Baseline: RED. C never appears in 33 s, and a USB-MSC read times out.
- hub: usb-hub on root port 1, stick A on 1.1. The hub enumerates full-speed.
  - Fixed: PASS. A is seen at boot, gone 2.0 s after device_del, and back as /dev/sdb 1.0 s after re-plug ("port 5.1").
  - Baseline: RED. The log says "no hub support yet", and A is never visible.
- hid: kbd1, kbd2, mouse1. Keys are injected without "device" (QEMU has no console binding), so they go to the newest keyboard, and bytes are counted on /dev/input/event0.
  - Fixed: PASS. The removal is logged, kbd2 keeps delivering, and the re-plugged kbd1 enumerates and delivers.
  - Baseline: RED at re-plug: no enumeration within 10 s and no keys.
- Baseline images were main's from 10-23 23:18, which predate the usbunplug merge, so unplug is also red there.
- Full aarch64 regress: all suites passed except nettag2test unbound_recv, a known flake that passed on re-run. x86_64 release build OK.
- Build note: relibc master eb01acd9 (another lane, sys_uio) did not compile at the time, so this lane built against a detached relibc worktree at aa2472fd (LEANDROS_RELIBC_DIR).

## Open
- Per-device evdev nodes: all USB keyboards feed the shared event0, and mice go through rel_pointer. A per-device node that disappears on unplug, with readers getting ENODEV, needs dynamic devices in servers/evdev (MAX_DEVICES=4, static slots). Not done.
- Not tested: SuperSpeed hubs, hubs on real hardware (high-speed TT path), the hub status-change interrupt endpoint, and the PSC event TRB wake-up.
