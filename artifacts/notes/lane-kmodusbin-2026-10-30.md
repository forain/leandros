# lane/kmodusbin (W2-6): input class kapi, VIRTIO_KEYBOARD as a module

Status: PARTIAL. Delivered: input kapi + VIRTIO_KEYBOARD tristate. Deferred: USB bulk/intr kapi, USB_HID, USB_STORAGE, BT (steps a, c, d).

## What
- `lib/kmod-abi/src/input.rs`, `kernel/src/kmod/exports/input.rs`: `kapi_input_register` (claims evdev node 0 keyboard / 1 pointer, RegKind::INPUT=12),
  `kapi_input_event`, `kapi_input_set_caps`. NEXPORTS 83 -> 86 (counted from the table); ktest kmodkapi has bad-arg cases.
- `drivers/src/virtio_keyboard.rs` goes through `kenv` (pci, irq with MsixTable, input), same pattern as virtio_net; `drivers/src/stubs/virtio_keyboard.rs`
  keeps init/poll_events/vq_census for the timer/boot callers. CONFIG_VIRTIO_KEYBOARD is tristate; `modules/virtio-keyboard` (alias pci 1af4:1052),
  polled from `kapi_tick_register`.
- `scripts/kmod/autoloadtest.sh <arch> virtio_keyboard`.

## Evidence (aarch64)
- autoloadtest virtio_keyboard: PASS (autoload, event0/event1 claimed, /proc/modules, blacklist).
- ktest=kmodkapi: 86 exports, input bad-arg cases ok.
- NOT done: typing test through the module (no event-injection check in autoloadtest); stats hook / vt_handoff_stats are no-ops in the module build.

## Left (handoff to next session)
- W2-6a: usb bulk/intr kapi (`kapi_usb_intr_*`, `kapi_usb_bulk`), boot HID as usb::class driver, USB_HID onto input kapi.
- W2-6c: USB_STORAGE onto bulk kapi + blk class (relocate usb_storage::sleep_until, usb::tick).
- W2-6d: BT HCI kapi.
- Merge note: kmodwifi may also add RegKind 12 / exports; keep main's values, renumber INPUT, recount NEXPORTS.

## W2-6a: USB_HID as a loadable module (lane kmodusbin2/3)
- DELIVERED: CONFIG_USB_HID=m. modules/usb-hid (slot usb_hid) binds boot keyboards/mice via the `usb 03 01 01/02` alias, opens the interrupt pipe over new kapi_usb_intr_open/poll/out/close, and registers per-device evdev nodes (kapi_input kinds 2/3, kapi_input_unregister/_event_console/_leds/_repeat, rel pointer). kapi exports 87 -> 97.
- Shipped configs keep USB_HID=y (unchanged boot-keyboard path).
- Evidence: aarch64 default, x86_64 default and x86_64 USB_HID=m build; ktest=kmodkapi PASS on both arches; autoloadtest usb_hid PASS (aarch64); usbunplugtest hid phase with USB_HID=m (aarch64): keys on per-device node, unplug/replug, ENODEV/POLLHUP all PASS; usbunplugtest all with USB_HID=y PASS; kmod selftest.sh aarch64 PASS.
- Test change: the (e) name check accepts the module's generic "USB HID keyboard".
- Limits / deferred: module names devices generically (not the product string); only boot HID interfaces via intr_open; =y path is still handled in xhci enumeration (HID class-driver path for =y not done); kapi_usb_bulk, USB_STORAGE (W2-6c), BT not started.
