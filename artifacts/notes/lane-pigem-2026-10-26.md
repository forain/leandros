# Lane pigem (2026-10-26): Raspberry Pi 5 Ethernet driver (Cadence GEM in RP1 + BCM54213PE)

Status: DELIVERED as code + host tests; hardware-unproven (needs lane pipcie's RP1 probe and a Pi boot).

## What landed
- `drivers/src/macb_hw.rs`: hardware-independent part. GEM register map (Linux macb.h names), 64-bit
  descriptors (16 B: addr lo, ctrl, addr hi, reserved; DMACFG.ADDR64), RX/TX rings, clause-22 MDIO
  over MAN/NSR.IDLE, BCM54213PE soft reset + RGMII delays (AUXCTL misc RGMII_SKEW_EN = RX delay,
  shadow 0x1c clock-control GTXCLK_EN = TX delay, as Linux `bcm54xx_config_clock_delay`), 10/100/1000
  advertisement, autoneg, link resolution, `LinkWatch` (link change -> NCFGR SPD/FD/GBE + NCR RE|TE),
  `fdt_find_gem` (local-mac-address, phy-mode, `ethernet-phy@` reg).
- `drivers/src/macb.rs`: kernel glue, netif registered exactly like r8169/e1000e (`eth0`, DHCP via
  the net server). Driver thread does PHY bring-up and polls the link (250 ms down / 1 s up).
- `boot::device_tree::recorded_dtb()`: the recorded DTB as a byte slice.
- `drivers/macb-tests`: 8 host tests (`cd drivers/macb-tests && cargo test --target aarch64-apple-darwin`).

## Seam for lane pipcie (the RP1 probe must do this, before `netdev::init` in kernel/src/init.rs)
```rust
let va = mm::kva::ioremap(bar1_phys + drivers::macb::RP1_GEM_OFFSET /*0x10_0000*/,
                          drivers::macb::RP1_GEM_SIZE /*0x4000*/, mm::kva::CacheMode::Uncached)?;
drivers::macb::rp1_attach(drivers::macb::Rp1Gem {
    mmio_va: va, dma_offset: drivers::macb::RP1_DMA_OFFSET /*0x10_0000_0000*/, irq_wired: true });
// RP1 interrupt demux, RP1 IRQ 6 (drivers::macb::RP1_GEM_IRQ, level-high):
drivers::macb::rp1_isr(0);
```
Derivation (rpi5.dts): `rp1_eth: ethernet@100000 { reg = <0xc0 0x40100000 0 0x4000>; interrupts = <6 4>; }`,
rp1 `ranges = <0xc0 0x40000000  0x2000000 0 0  0 0x410000>` (BAR1 offset 0), pcie2 `dma-ranges`
PCI 0x10_0000_0000 -> CPU 0 => DMA address = phys + 0x10_0000_0000. With `irq_wired: false` the
net daemon polls. RP1 clocks (pclk/hclk/tsu/clk_eth) and the PHY reset GPIO (RP1 GPIO 32) are left as
firmware set them; pipcie/a later lane must enable the clocks if firmware does not.

## Evidence
- Host tests 8/8; mutation check: dropping GBE from the NCFGR clear mask and dropping WRAP on RX
  release made 3 tests fail (register_values, link_change_down_then_100_full, rx_ring_init_and_wrap).
- `scripts/check-rpi5.sh` clean (rpi5 and CHECK_FEATURE=raspi4b), no macb warnings.
- aarch64 release build + QEMU virt regress (lock, run id pigem-aarch64): overall PASS, 0 boots failed; no `[macb]` line on virt (inert).

## Inertness
Nothing runs unless `rp1_attach` is called; `init` checks MID (0 / all-ones / non-GEM -> one line,
off), 64-bit DMA support (DCFG6.DAW64), and allocation; every failure logs one `[macb] ...` line.
`macb=0` on the command line disables it. /proc/net/nic shows `macb: <state>`.

## Expected on the next Pi boot (once pipcie attaches)
```
[macb] attached: regs at 0x..., dma offset 0x1000000000, irq RP1 6, dtb node found
[macb] GEM MID 0x..., N queue(s), MAC 2c:cf:67:..:..:.. (from dtb)
[macb] PHY BCM54213PE id 0x600d84a2 at 1, rgmii-id, autoneg started
[macb] link up 1000 Mbit/s full duplex (NCFGR 0x...)
```
then eth0 DHCP lease from the net server. Without pipcie: no `[macb]` line at all.

## Open / risks (unverified on hardware)
- USRIO=1 (RGMII) and AMP=0x10808 (`cdns,aw2w/ar2r-max-pipe = 8`, `use-aw2b-fill`) follow the
  Raspberry Pi Linux tree from memory; ISR is written back after read (covers clear-on-write).
- If the firmware leaves `local-mac-address` zero and SA1 empty, a locally administered MAC is generated.
- No TX completion interrupt; TX slots are reclaimed on send/poll and the thread re-rings TSTART.
