# lane/piroot 2026-10-26 — Pi 5 boot waited 30 s for its root (openbugs #102): FIXED

## Cause (from /tmp/pihw4-serial.log, main c89fcf6d)
- The SD card was probed and present before init ran; the wait was not SDHCI init nor a late disk.
- RPI5BOOT had no cmdline.txt (`[sdcard] /cmdline.txt not found`), so the firmware appended its
  Raspberry Pi OS default: `console=ttyAMA0,115200 console=tty1 root=/dev/mmcblk0p2 rootfstype=ext4 rootwait`.
- `drivers/src/blkdev_sdhci.rs::resolve_root` only knew `/dev/vdX`, so `root=/dev/mmcblk0p2` -> NotFound
  -> ENODEV -> init's mount_root retried 30 s ("Waiting for the root device (0..29 s)"), then
  "mount /dev/root failed (errno 19), falling back to /dev/vdb", which mounted the same card at once.

## Fix
- Kernel (`blkdev_sdhci.rs`): the card is probed synchronously and nothing hotplugs on this board, so a
  `root=` that names nothing now never will. `/dev/vdX` and `LABEL=<f2fs label>` (new) resolve as before;
  any other hint is logged once (`[BLK] root=... names no device on this board; using the SD card`) and the
  card at index 1 is used if it holds f2fs. No wait.
- `scripts/prepare-rpi5-sdcard.sh` (direct boot) now writes `cmdline.txt` = `root=LABEL=leandros-root`
  (label set by mkfs-f2fs-populated.py), so the firmware default root= is never used.

## Deploy to an existing card
- RPI5BOOT: replace `kernel.img` (and optionally `initrd.cpio`, unchanged) from `$SP/piroot/`, and add
  `cmdline.txt` containing `root=LABEL=leandros-root`. Either change alone removes the 30 s wait.

## Not verified
- Real Pi boot (not touched by this lane). QEMU raspi4b gets no firmware cmdline, so the foreign-root= case
  cannot be reproduced there; the generic (non-SD) resolver is unchanged — explicit root= that matches nothing
  still waits, as intended for USB enumeration. aarch64 regress (generic build): overall PASS, 0/1 boots failed.
