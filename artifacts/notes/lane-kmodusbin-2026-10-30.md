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
