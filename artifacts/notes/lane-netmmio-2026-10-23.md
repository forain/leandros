# lane/netmmio (openbugs L8.7 remainder) - FIXED

drivers/src/virtio_net.rs now uses `leandros_lib::mmio` (`mmio_read`/`mmio_write`) for virtio common cfg,
device_status, feature registers, queue setup, notify doorbell, ISR, device cfg status and the x86 MSI-X
table/vector setup. The local `cfg_read8` helper is removed. RAM ring/descriptor/used-index accesses stay volatile.

Evidence: `cargo check -p drivers --target x86_64-unknown-none --release` ok; aarch64 build-all ok;
under HVF on aarch64: nettest, udptest, netctltest all RC=0, fails=0.
