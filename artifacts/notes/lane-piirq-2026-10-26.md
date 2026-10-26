# lane/piirq 2026-10-26 (openbugs #101: Pi 5 UART RX IRQ never fires) - PARTIAL (diagnostic; no provable code bug)

## Checked and ruled out (code + BCM2712 DTB + QEMU)
- (a) INTID: decoded scratchpad hw/bcm2712-rpi-5-b.dtb with dtc: `uart10: serial@7d001000` has
  `interrupts = <0x00 0x79 0x04>` (SPI 121, level-high), no own interrupt-parent; root interrupt-parent = phandle 1 =
  `arm,gic-400` at 0x7fff9000. INTID 153 is correct.
- (c) cfg: `SPI_PL011 = 32 + 121` under `rpi5`; the same cfg selects the GIC base, so a feature mismatch would also break the timer.
- (d) IMSC: `uart::reinit` writes IMSC = RXIM|RTIM (0x50) on rpi5 too (only IBRD/FBRD/LCRH are inherited).
- (b) GIC programming: enable_spi sets ICFGR level, IPRIORITYR 0xA0, ITARGETSR = BSP banked mask, ISENABLER; GICD_CTLR=1,
  GICC_CTLR=1, PMR=0xFF. IGROUPR is never written on v2 (NS writes are WI anyway).
- QEMU `-M raspi4b` (GIC-400 model, same INTID 153, GICv2 code path): `[uartirq] init ... en 1 tgt 1 prio a0 cfg 0
  C.ctlr 1 C.pmr ff imsc 50`, and `[uart] ... irqs 1 tick 0`: the v2 path delivers INTID 153. QEMU's GIC has no
  Security Extensions (TYPER 0x66), the real GIC-400 does: the remaining difference is firmware-owned state.

## Added (d4a3ac3a)
- `[uartirq] init ...` once after gic::init, and `[uartirq] rx-pending ...` once: snapshot taken in the cpu0 tick the
  first time it finds RX bytes un-taken by the IRQ (before draining), printed by kiostat. Fields: CurrentEL, GICD_CTLR/TYPER,
  en/pend/act/grp (bits for INTID), tgt/prio bytes, cfg (2 bits), GICC_CTLR/PMR/RPR/HPPIR (read-only, no IAR), PL011 IMSC/RIS/MIS/FR/CR.

## Reading the next Pi capture
- en 0 / tgt 0 / prio 0 at init (writes did not stick) => SPI is Secure Group 0 in the firmware's IGROUPR (NS RAZ/WI); grp reads 0.
- en 1, tgt 1, rx-pending shows pend 0 with ris/mis bit4|6 set => the PL011 line does not reach the GIC.
- pend 1, en 1, tgt 1, C.hppir 99 => GIC presents it; then CPU interface/EL (EL 2, C.ctlr) is the problem.
- mis 0 with ris set => IMSC lost (something rewrote it); imsc shows.
- QEMU aarch64 regress PASS (tests-piirq).
