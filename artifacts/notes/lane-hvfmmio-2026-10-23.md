# lane/hvfmmio 2026-10-23 (openbugs L8.7) - PARTIAL (core DONE)

Problem: `read_volatile`/`write_volatile` on aarch64 MMIO may compile to writeback
forms (`ldrb w8, [x19, #2]!`, ISV=0) that HVF cannot emulate (QEMU aborts).

Fix: new shared `lib/src/mmio.rs` (`leandros_lib::mmio`): `read/write` generic over
u8/u16/u32/u64 plus `read8..64`/`write8..64`; single inline-asm `ldr*/str*` with plain
`[reg]` on aarch64, volatile elsewhere. Method spellings `p.mmio_read()` / `p.mmio_write(v)`
(traits `MmioRd`/`MmioWr`) so `.read_volatile()` sites convert by rename.

Converted (device MMIO only; RAM rings/used/avail/desc/event buffers left volatile):
virtio_gpu, virtio_blk, virtio_rng, virtio_keyboard (its local cfg_read8/cfg_write8 now
delegate to the shared helper), snd, pci (ECAM + BAR), irq (MSI-X table/PBA), sdhci,
rpi_mailbox, arch/aarch64 gic + uart + rtc, kernel/kexec/devreset (PCI cfg).
Already safe, untouched: hda.rs, nvme.rs (local ldr/str helpers), xhci/regs.rs.

NOT touched: drivers/src/virtio_net.rs (lane netirq is editing it; its `cfg_read8` copy
at ~:561 and the volatile accesses on its common/notify/ISR regs should be converted after
that lane merges: `use leandros_lib::mmio::*`). Also unconverted: x86-only drivers
(e1000e, r8169, iwl*, i915, nvidia*), ktest, drm_device_interface (RAM/scratch),
framebuffer (normal memory), arch/aarch64 smp spin-table.

Evidence: aarch64 + x86_64 release builds compile; aarch64 regress on HVF: all green except
host:evherd/pollnarrow/listentag/nettag2 which failed in the full run (0/1) and passed
on immediate rerun (4/4, load flakes). objdump writeback counting was inconclusive
(whole-kernel count 14 both before and after; those are RAM/stack forms) - no per-function proof.
