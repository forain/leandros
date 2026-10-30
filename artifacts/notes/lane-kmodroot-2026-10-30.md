# lane/kmodroot (2026-10-30): root-on-module tests, SDHCI as a module

Status: DELIVERED (QEMU gates; the Pi 5 =m kernel is compile-only, needs a real Pi trial).

## What
- `scripts/kmod/rootmodtest.sh <arch> nvme|usb_storage|sdhci`: builds with the driver =m, puts the ROOT FILESYSTEM disk
  (LABEL=leandros-root, data0) on that bus via new `LEANDROS_ROOTFS_BUS=nvme|usb` (driver.py; the older LEANDROS_ROOT_BUS moved the
  boot disk only, so root never depended on the module). nvme needs `nvme.rw=1` (NVMe is read-only by default).
  Nothing broke in early-boot ordering, the usb-storage 3 s ready wait or the root wait: no fixes needed there.
- Main arrived with NEXPORTS 109 vs a 118-entry table (drm+bt merge): recounted to 118 (ktest too).
- SDHCI is tristate: `drv-sdhci` (=y, unchanged built-in blkdev_sdhci path) / `mod-sdhci` (=m, `modules/sdhci`, same sdhci.rs through
  kenv, card registered via kapi_blk_register as class "vd", of: brcm,bcm2712-sdhci + brcm,bcm2711-emmc2, in modules/BOOT, slot 304).
  Board features reach the module crate via KMOD_BOARD (build-all -> kmod-build.sh). New configs/raspi4b_defconfig (build-all --raspi4b).
- Runtime registry root guess: a lone whole "vd" disk with f2fs at index 0 is accepted (the module's card is vda, the built-in is vdb).
- kmod-kapi blk env: `sched::irqctx::in_irq` (false), `sched::kthread::cond_resched`.
- Module reports 256 GiB capacity (driver does not read C_SIZE; info() says 0).

## Evidence (aarch64, QEMU)
- rootmodtest nvme PASS, usb_storage PASS, sdhci (-M raspi4b) PASS; raspi4b =y baseline still boots (root vdb); --rpi5 SDHCI=m builds, aliases emitted.
- x86_64: not run for sdhci (aarch64-only); nvme/usb rootmodtest x86_64: see merge report.

## Open
- Pi 5 hardware trial of SDHCI=m (SD root through the module). Shipped configs stay =y.
