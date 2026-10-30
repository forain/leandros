# lane/kmodbt (W2-6d): Bluetooth USB transport as a loadable module

Status: DELIVERED (BT tristate; shipped configs keep BT=y).

## What
- Kernel kapi (`kernel/src/kmod/exports/hci.rs`), NEXPORTS 100 -> 109 (recounted from the table):
  `kapi_hci_register/rx/tx_pop/ctl/unregister` (claims /dev/hci0, wraps the in-kernel `bt::hcidev`),
  `kapi_usb_bt_open/poll/send/close` (wrap `BtUsb`: event intr-IN, ACL bulk IN/OUT, class command on EP0).
  Deviation from the design sketch (`KHciOps.send` callback): the TX side is polled (`kapi_hci_tx_pop`), the
  transport's pump thread already owns the controller; no extra kernel thread.
- `CONFIG_BT` tristate (default y). `=m` -> feature `mod-bt` = `usb/bt-module`: the enumerator no longer feeds
  E0/01/01 to the built-in btusb thread; the function goes through `usb::class` (alias `usb * * e0 01 01`).
  `=y` is the unchanged built-in path (`drivers::bt::usb::start` finds no function under =m and spawns nothing).
- `modules/bt`: same `btintel.rs` / `usb_reasm.rs` source via `#[path]`; probe opens the pipe and spawns the
  `btusb` thread (Intel set-up -> `hci::register` -> pump). HCI core, Pi UART transport, btd/bthid stay built in.
- Test: `scripts/kmod/autoloadtest.sh <arch> bt` with `usb.alias_selftest=bt` (synthetic e0/01/01 interface;
  QEMU has no USB BT controller). The module loads, registers its class driver and stays resident (no probe).

## Limits
- No real controller exercised (laptop Intel 8265 trial is the orchestrator's: deploy a BT=m kernel, expect `[btusb]` lines).
- Detach is passive: the pump sees `Gone` on its next poll and marks /dev/hci0 dead; the kthread parks.
- Probe failure on a device without a complete HCI interface returns -19 (kernel keeps the interface).

## Evidence
See the end of this file (filled after the gates ran).
- aarch64 default regress on the box (6fb4a7a1): overall PASS, 0 boots failed; selftest.sh aarch64 0 FAIL;
  ktest=kmodkapi both arches 0 FAIL (109 exports); autoloadtest bt PASS (aarch64); x86_64 default and BT=m builds ok
  (bt module in initrd); `--rpi5` aarch64 build ok. main had not moved (no merge conflicts with kmoddrm yet).
