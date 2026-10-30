# lane/kmodstor (W2-6c): USB_STORAGE as a loadable module

Status: DELIVERED (aarch64 + x86_64 =m build; QEMU gates on aarch64).

## What
- Kernel kapi: `kapi_usb_bulk_open(handle, iface) -> pipe`, `kapi_usb_bulk(pipe, op, arg, buf, len, timeout_ns) -> isize`
  (op 0/1 bulk OUT/IN, 2/3 clear-halt IN/OUT, 4/5 class control OUT/IN with arg = request | value<<8 | index<<24),
  `kapi_usb_bulk_close(pipe)`. Backed by `usb::xhci::open_bulk_on` (same Configure-Endpoint sequence as the
  built-in enumerator, run after the class function's SET_CONFIGURATION). NEXPORTS 97 -> 100 (recounted from the table).
- `CONFIG_USB_STORAGE` is tristate (default y). `=m` -> feature `mod-usb-storage` = `usb/msc-module` (the enumerator
  no longer claims bulk-only interfaces, they go through `usb::class` / the `usb * * 08 06 50` alias) +
  `drivers/usb-storage-module`. `=y` is the unchanged built-in path (`drv-usb-storage` now gates `usb_storage.rs`).
- `drivers/src/stubs/usb_storage.rs` (=m/=n): same entry points, plus a small `usb-hotplug` kthread so the xHCI
  hot-plug scan, class detach callbacks and slot release keep running.
- `modules/usb-storage`: the same SCSI/BOT source (`drivers/usb/src/msc`), transport = kapi bulk pipe, disk through
  `kapi_blk_register` (removable, class sd), detach drains the in-flight request, `kapi_blk_remove`, closes the pipe.
  Listed in `modules/BOOT` (appended to modules.load when built =m).
- Relocations (binding decisions): `usb_storage::sleep_until` moved to `drivers::kenv::sleep_until` (callers klog,
  usb_hcd, usb_storage re-export). `usb::tick` was already in `usb::class::tick`/`tick_register` (done by kmodusbin2).
- Test: `scripts/kmod/autoloadtest.sh <arch> usb_storage` (QEMU usb-storage, 8 MiB image with a marker in block 3).

## Evidence
See the end of this file (filled after the gates ran).

## Limits / left
- The module is lean: no IoWindow (atomic-context I/O), no usbms.* latency knobs, no hungtask `inflight`/lock_owner,
  no iostat counters, no background poll of not-yet-ready LUNs (3 s ready wait, then the interface is declined).
  Under `=m` the hungtask/iostat USB sections are empty (stub). Shipped configs keep `USB_STORAGE=y`.
- UAS-only devices unchanged (unsupported). S3 resume re-address (`persist`) is not set for the module path.
- USB root with `=m` (LEANDROS_USB_STICK / ROOT_BUS=usb) not exercised here; the module is in modules/BOOT for it.
