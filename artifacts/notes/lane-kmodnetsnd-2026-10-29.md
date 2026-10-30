# lane kmodnetsnd (kmodW2 W2-4), session 1, 2026-10-29: PARTIAL (VIRTIO_NET done; sound + HDA remain)

Branch lane/kmodnetsnd, not merged. Handoff: leandros-artifacts/orch/handoff/kmodnetsnd.md (design for the sound kapi, pipewire attach-late, VIRTIO_SND, HDA).

## Done: VIRTIO_NET=m
- `VIRTIO_NET` is tristate (default y); `modules/virtio-net` includes drivers/src/virtio.rs and virtio_net.rs unchanged apart from the kenv seam; alias pci 1af4:1041 and 1af4:1000; one netif per device (ctx = device index).
- virtio_net.rs goes through `crate::kenv::{pci,irq,mm,sched,netdev}`. x86 MSI-X now uses `irq::MsixTable` (built-in: drivers::irq::MsixTable; module: kapi_irq_request_msix); aarch64 INTx uses `PciFunction::intx_line` + `request_irq(Line)`.
- find_msix/find_isr/bar64 relocated to drivers/src/virtio.rs (virtio_keyboard updated, stub duplicates removed). kmod-kapi: pci::find_all_devices, pci::serial_debug_hex, irq::MsixTable.
- `WIFI_SIM depends on VIRTIO_NET=y`. `scripts/kmod/autoloadtest.sh <arch> virtio_net` added.

## Evidence
- autoloadtest virtio_net: aarch64 all ok; x86_64 autoload, MSI-X armed, DHCP, nettest, /proc/modules, kmodctl all ok (blacklist pattern then fixed for both alias rows; not re-run).
- Features diff vs main: 0 lines on aarch64, x86_64, x86_64_laptop, rpi5 defconfigs. selftest.sh 0 FAIL.
- Default (=y) aarch64 regress under lock.sh qemu: PASS (0 boot failures).
- x86_64 box regress (lane/kmodnetsnd 0e95fc53): queued, result not seen by this session.

## Open
- procnet rx_irq_count reads the stub (0) with VIRTIO_NET=m.
- Sound kapi, VIRTIO_SND=m, HDA=m, laptop HDA trial: not started (see handoff).
