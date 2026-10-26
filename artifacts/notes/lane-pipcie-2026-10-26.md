# lane/pipcie — BCM2712 pcie2 root complex + RP1 probe (2026-10-26)

Status: PARTIAL (implemented, host-tested, QEMU virt regress green; NOT hardware-verified).

## What
- `drivers/src/brcmstb_hw.rs` — pure register map + math (Linux `pcie-brcmstb.c`, BCM2712/7712 layout):
  outbound windows, RC_BAR size encoding + UBUS remap, EXT_CFG_INDEX/DATA addressing, BAR placement,
  bridge window, MDIO packets, 54 MHz PLL table, MIP0/RP1 MSI-X routing, RP1 GPIO layout.
- `drivers/src/brcmstb_pcie.rs` — bring-up: rescal pulse, bridge SW_INIT (brcmstb-reset line 44),
  PERST#, SerDes IDDQ off, PLL over MDIO, PHY_CTL_15 PM clock, MISC_CTRL, UBUS error suppression
  (failed reads return all-ones, no SError), inbound RAM (PCI 0x10_0000_0000 → 0, 64 GiB) and MIP
  doorbell (PCI 0xff_ffff_f000 → 0x10_0013_0000), outbound (0x1f_0000_0000 → PCI 0; 0x1c_.. → 0x4_..),
  gen2/x4/ASPM-L1, link-up poll (100 ms settle + ≤200 ms), RC bus 0→1, RP1 BARs largest-first from
  PCI 0 (BAR1 at 0 as the DTB `rp1` ranges assume).
- `drivers/src/pci.rs` — `set_cfg_backend(f, mem_cpu_offset, max_bus)`: non-ECAM config backend; scan,
  `PciFunction` access, `map_bar` (bus→CPU via `bus_to_cpu`) all route through it. No backend = old path.
- `drivers/src/irq.rs` — `request_irq_edge(intid, ..)` (GIC SPI edge).
- `drivers/src/rp1.rs` — probe: map BAR1, SYSINFO chip id, MIP0 init (edge, host-unmasked, VPU-masked),
  MSI-X table entry n → MIP0 vector n (all masked), GEM PHY reset pulse (GPIO 32, active low, 5 ms),
  `macb::rp1_attach` (polled unless `rp1.irq=1`).

## RP1 interrupt API
`rp1::request_irq(n, level, handler, ctx)`: RP1 source n → MSI-X n → MIP0 vector n → GIC INTID 160+n (edge).
Level sources get MSIX_CFG.IACK_EN, and a trampoline writes IACK after the handler. Also `rp1::irq_ack(n)`,
`rp1::free_irq(n, h)`, `rp1::bar1()`, `rp1::dma_addr(pa)` (= pa + 0x10_0000_0000).

## Knobs
- `pcie.rpi5=1` (default OFF): runs all of the above from kernel/src/init.rs after blkdev::init, before netdev.
  Without it nothing new executes. Bounded: ≤ ~0.35 s total; every failure is one `[pcie] FAIL ...` line.
- `rp1.irq=1`: GEM interrupt over MSI-X instead of polling.

## Expected Pi boot lines (cmdline `... pcie.rpi5=1`)
```
[pcie] rc rev 0x...
[pcie] rc link up gen2 x4
[pcie] 0000:01:00.0 1de4:0001 bar1=0x1f00000000 ...
[rp1] chip_id=0x20001927 platform=0x...
[rp1] msix vectors=0x... via mip0 spi 128
[rp1] eth phy reset pulsed (gpio32)
[macb] attached: ... irq none (polled) ...
[pcie] state=up
```
Failure forms: `[pcie] FAIL ioremap|rescal timeout|mdio timeout`, `[pcie] FAIL link down status=0x..`,
`[pcie] FAIL no RP1 at 01:00.0 id=..`, `[rp1] chip_id=.. (unexpected)`, `[rp1] FAIL ...`; then `[pcie] state=<x>`.

## Evidence
- Host tests `drivers/brcmstb-tests` 8/8 (window values derived from the firmware DTB pcie2 `ranges`/`dma-ranges`).
- `scripts/check-rpi5.sh` rpi5 + raspi4b clean (no warnings in new files).
- aarch64 QEMU virt regress PASS (before and after `git merge main`).

## Open / unverified
- RP1 clocks (pclk/hclk/tsu/clk_eth): NOT programmed (needs an RP1 clock driver, clk-rp1.c). If RP1's
  power-on defaults leave the GEM unclocked, macb will read zeros/time out — next lane.
- MIP CFG polarity (~0 = edge) and RP1 MSIX_CFG bit layout are from memory of Linux; unverified.
- DTB dma-range PCI 0 → 0x1f_0000_0000 (4 MiB) not programmed (RP1-internal DMA to its own BAR).
- vdm-qos-map / TC QoS, L1SS not configured. Firmware `pciex4_reset=0` inherit path not used.
