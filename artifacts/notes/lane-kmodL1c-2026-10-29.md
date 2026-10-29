# lane/kmodL1c - misc driver gates (2026-10-29)

Gated (Kconfig symbol -> kernel feature -> stub in drivers/src/lib.rs, same public API):
- VIRTIO_RNG -> drv-virtio-rng  (stub: init() no-op, read() = 0)
- HDA -> drv-hda  (stub Hda: probe() = Err, capture_* = -ENODEV). The ARCH_X86_64 dependency was dropped from the
  Kconfig symbol because hda.rs is built on aarch64 too; otherwise aarch64 would silently become a stub.
- LAPINV -> drv-lapinv  (x86_64 only; stub run() no-op)
- I2C_HID -> drv-i2c-hid  (x86_64 only; gates i2c_hid + dw_i2c + i2c_acpi; stub i2c_hid: init() = false)

Not gated (deliberate):
- virtio_snd: lives in snd.rs, which is the pipewire backend type (42 uses); stubbing it is a bigger change.
- psmouse (PS/2, keyboard controller coupled, 15 uses incl. arch/x86_64), nvme (can be the root disk), uvc/bt
  (usb/bt trees; bt has 10 files), thermal (only no_mangle /proc hooks + rpi5 fan), acpi_power/power_sysfs/pci_pm
  (34/12/6 users incl. syscall paths) - candidates for a follow-up lane.

Bare `cargo build` of the kernel (not via build-all) has none of the drv-* features, so it builds the stubs.

## Gates
1. NOT byte-identical. aarch64 default (`LEANDROS_KERNEL_TAG=gate`, same-length worktree paths): kernel 6933584 (main) vs 6933576 (lane), sha differ.
   Symbol tables (hash-normalised) are identical except monomorphisation placement (a few drop_in_place instances move between
   the kernel and drivers crates, one extra RawVec<usb::uvc::Done>::grow_one); no strings differ. Cause: declaring new cargo features
   changes crate metadata/CGU partitioning. L0 matched only because it declared no feature. L1a/L1b will hit the same. x86_64 base hash not obtained.
2. N works: HDA, VIRTIO_RNG (and on x86_64 also LAPINV, I2C_HID) = n builds on both arches and boots (runtests /bin/vfstest PASS aarch64 + x86_64 TCG, no [RNG]/[LAPINV] output).
   Kernel size: aarch64 6933576 -> 6870984 (-62592 B), x86_64 6824560 -> 6698992 (-125568 B).
3. aarch64 default full regress: overall PASS.
