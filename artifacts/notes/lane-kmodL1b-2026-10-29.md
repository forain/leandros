# lane/kmodL1b - GPU / display Y/N gating (2026-10-29)

Symbols (drivers/Kconfig) -> kernel features: I915 -> drv-i915, VIRTIO_GPU -> drv-virtio-gpu, V3D -> drv-v3d
(each maps to a same-named drivers feature: i915, virtio-gpu, v3d). NVIDIA keeps its existing board-style
feature `nvidia` (BOARD_FEATURES in to-cargo.py); nothing to fold. hdmi_* not gated (see below).

## What "off" does
- i915 =n: `drivers::i915` is replaced by drivers/src/i915_stub.rs (same API: BootEnv, run, kexec_quiesce,
  pm_suspend/resume, proc_text, set_tmpfs_census, prime_dedup, ptecheck_*, backlight::{get,set}, and the
  emit/emit_logonly/now_ns/delay_us helpers acpi_power uses). No call site (syscall.rs, pm.rs, power.rs, init.rs,
  servers/drm, power_sysfs, acpi_power) changed. The whole i915/ tree (~14k lines) drops out.
- virtio_gpu =n: module stays compiled (drm_device_interface / kms.rs use its types and statics in ~40 places),
  but `virtio_gpu::init()` returns early so the device is never probed and VIRTIO_GPU stays None. Only partial
  size win; a real facade needs a larger refactor of drm_device_interface.rs.
- v3d =n: kernel/src/init.rs skips `drivers::v3d::probe_boot` (rpi5 only). v3d/kms_v3d logic stays compiled
  (drm_device_interface references v3d::submit/mmu). Partial win.
- hdmi_probe/hdmi_audio: not touched (rpi5-only, not in the L1b symbol set that exists in Kconfig beyond HDMI_AUDIO, which
  is left for a later lane).

## Kconfig change
I915 no longer `depends on ARCH_X86_64`: i915 was compiled into the aarch64 kernels before too, so keeping it
y by default there preserves today's code. So i915=n is testable (and builds) on aarch64 AND x86_64.
Only the aarch64 =n path was boot-tested (QEMU aarch64 has no i915 device anyway).

## Gates (new gate 1 per coordinator: no byte identity)
Kernel-only builds mirroring build-all (LEANDROS_KERNEL_TAG=gate), main 6e972e3d vs branch, aarch64 / rpi5 /
x86_64 / x86_64_laptop defconfigs, kernel and kernel-direct: llvm-nm -C symbol sets (hash-stripped) equal except
generic drop_in_place/grow_one instances and `.NNNN` local-suffix renumbering; strings differ only in merged
literal-pool order. Sizes of all-Y vs main: within -40 bytes everywhere (aarch64 6933584 -> 6933552 ...).
=n builds (I915, VIRTIO_GPU, V3D all n) compile for aarch64, aarch64 rpi5, x86_64:
  standard kernel: aarch64 6933552 -> 6524280 (-409 KB, -5.9%); x86_64 6824536 -> 6430904 (-394 KB, -5.8%);
  rpi5 7295944 -> 6834528 (-461 KB, -6.3%).
Boot: all-n config (I915, VIRTIO_GPU, V3D off) built with build-all and booted in QEMU: aarch64 (/bin/true, vfstest PASS)
and x86_64 (/bin/true, vfstest PASS), no panic. Default aarch64 full regress under the qemu lock: all pass except one
/bin/nettest tcp_keepalive_probes (timing flake; /bin/nettest alone re-run passes). x86_64 default not booted (lane rule).
Pitfall: a regress run killed by ENOSPC leaves its QEMU holding the image ("Failed to get write lock"); pkill it.
