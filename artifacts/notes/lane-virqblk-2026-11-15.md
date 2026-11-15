# lane/virqblk 2026-11-15
virtio-blk completion wait is hybrid: spin virtio_blk.spin_us (default 50 us), then park on the completion IRQ; virtio_blk.irq=0 = pure spin, virtio_blk.spin_us=0 = pure park; 64 parked waits without IRQ fall back to polling.
Measured aarch64 HVF (dd 256MiB conv=fsync x3 per boot, 3 boots per mode, shared Mac so noisy), qemu CPU-s per 768 MiB: hybrid 2.7/3.4/3.4, spin 6.9/5.5/5.9, park 4.1/4.3/4.0. Throughput MB/s mean: hybrid ~214, spin ~170, park ~116.
Pure park was the slow/costly path found in virqblk3 (wfi/IPI wake latency); hybrid avoids it for short requests. x86_64 KVM unmeasured.
