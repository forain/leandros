# lane/pcfixes 2026-10-08

Small fixes from the first real-PC boot logs (pc-boot1.log, nvrm-boot1.log).

## USB mass storage flush
- Real stick 13fe:4300 answers SYNCHRONIZE CACHE(10) with CHECK CONDITION, sense key 0. `ScsiDisk::flush` now treats NO SENSE, RECOVERED ERROR and ILLEGAL REQUEST as success, remembers it (`sync_cache_unsupported`) and never issues the command again; `usb_storage.rs` logs once.
- Host tests: `flush_no_sense_and_recovered_are_success_and_cached`, `flush_real_error_still_fails` (run: `cargo test --manifest-path drivers/usb/msc-tests/Cargo.toml --target aarch64-apple-darwin`).
- Poweroff "0 f2fs volume(s) committed ... (the rest were already read-only)" is benign: init remounts / read-only first, and the f2fs remount-ro writes the CP_UMOUNT checkpoint. Verified with x86_64 `--usb-stick` style boot (driver.py + LEANDROS_USB_STICK): boot 1 sync+poweroff, boot 2 logs "checkpoint ver 146 is a clean unmount", zero flush warnings. (QEMU's usb-storage accepts SYNCHRONIZE CACHE, so the no-sense path is covered by the host test only.)

## nvprobe
- VRAM: 0x100ce0 is a per-FBPA amount (0x208 = 2 GiB), the 8 GiB RTX 3070 has 4 FBPAs. Now prints per-FBPA size, FBPA count (0x022438), floorsweep mask (0x022554) and an estimated total, labelled UNVERIFIED (no nouveau source was available offline; register offsets from memory of gp100/ga102 ram probe). Needs checking on the PC.
- `nvprobe.dumpvbios=1`: base64 now goes to the klog ring only via new `klog_write_only` (kernel/src/main.rs); no UART/framebuffer, so no ~20 s boot delay. Note the ring is 2 MiB; a 1.4 MB dump can push older early-boot text out before flush.

## HID
- Unsupported-device summary appears once from the driver ([USB] line) and twice from init's "USB devices:" list, before and after the NO GPU banner; intentional, left alone.

## Tests
msc host tests green; aarch64 and x86_64 regress 15/15; x86_64 USB stick boot twice.
