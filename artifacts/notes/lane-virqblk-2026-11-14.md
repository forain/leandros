# lane virqblk (2026-11-14): virtio-blk completion interrupt

Implementation: see handoff; knob virtio_blk.irq=0 restores the spin wait.
Fixes in finisher: walk_caps 5-tuple, KSYMTAB NEXPORTS 152->154. Both arches build (release).

## Evidence (aarch64, Mac HVF, single run each, noisy)
- Boot log: "[VIRTIO-BLK] INTx armed: INTID 0x24/0x25/0x26" and "first completion interrupt".
- dd 256 MiB conv=fsync write: default 236 MB/s, irq=0 (spin) 196 MB/s.
- dd read-back (page cache): default 464 MB/s, irq=0 1.9 GB/s (cached, not meaningful).
- No throughput regression on writes. CPU busy comparison not measured on Mac (HVF); see box regress.
