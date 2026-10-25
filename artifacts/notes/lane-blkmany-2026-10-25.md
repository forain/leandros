# lane/blkmany — 2026-10-25 — FIXED (openbugs #83)

## Root cause
`drivers/src/virtio_blk.rs` `probe_all()` capped virtio-blk devices at
`MAX_BLK_DEVICES = 8` and silently `break`ed past it. The default QEMU config
already attaches 3 disks (vda root, vdb, vdc home), so 8 extra data volumes
gave 11 PCI functions and the last 3 (vdi..vdk) were never probed: no /dev node,
`mount` -> "no such device". Not PCI/BAR/MSI (the driver polls; QEMU enumerated
all functions fine), not LABEL scanning, not the VFS mount table (interop mounts
one at a time).

## Fix
- `MAX_BLK_DEVICES` 8 -> 32; past the cap the probe now logs
  `[VIRTIO-BLK] device limit reached (32); skipping a virtio-blk PCI function`
  per skipped device instead of dropping it silently.
- Headroom downstream: kernel blkdev registry `MAX_ENTRIES` 64 -> 128 (disks +
  partitions), VFS `MAX_BDEV` 48 -> 96 (disks + 8 loops + partitions).
- `scripts/f2fslx/interop.py`: batches of 16 volumes per boot instead of 4.

## Evidence (aarch64, HVF)
- New `host:blkmanytest.py` (runtests `--suite blkmany`, N=12 extra disks, f2fs
  images, mount each by /dev path):
  - before: FAIL — mounted 5/12, vdi..vdo missing (8-device cap)
  - after:  PASS — 15 vd nodes, mounted 12/12; N=16: 19 nodes, 16/16
- aarch64 regress: overall PASS. x86_64 release build OK (not booted, per lane rule).

## Open
- interop.py batch change not re-run end to end (needs the box).
- Beyond 32 disks needs another raise; a virt machine's PCI bus 0 has 32 slots anyway.
