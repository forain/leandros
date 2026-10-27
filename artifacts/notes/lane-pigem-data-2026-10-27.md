# Lane pigem-data (2026-10-27): Pi 5 Ethernet link up but zero frames; RP1 xHCI commands time out

Status: FIXED in code (root cause found by reading the code against the boot log); not yet proven on hardware.

## Root cause (shared by GEM and xHCI)
RP1 never had PCI bus mastering on. The boot log shows `rp1_cmd=0x2` (memory only). `rp1::probe` calls
`enable_mem_and_bus_master()`, which does a 16-bit write to the Command register. The brcmstb controller
drops config writes smaller than a dword (lane pirp1-bar found the same thing on the root port and
left this one open). So every RP1 bus-master access was refused: GEM descriptor fetches and RX writes,
and the xHCI command ring ("command type 9 timed out", Enable Slot). MMIO still worked because it
only needs memory decode.

Fix: `brcmstb_pcie::cfg_access` turns 1- and 2-byte config writes into a dword read-modify-write
(`brcmstb_hw::sub_dword_write`, pure). In the Command/Status dword the RW1C Status half is written as
zero, so no error bit gets cleared by accident. MSI-X control writes now land too.
`[rp1] command=0x...` reads the result back.

## Ruled out by comparing with RPi Linux rpi-6.12.y (macb_main.c, broadcom.c, clk-rp1.c, rp1.dtsi)
Every one of these matches Linux:
- USRIO=1 (GEM_RGMII) at 0x0c
- AMP fields (ar2r 0..7, aw2w 8..15, aw2b 16)
- DMACFG (FBLDO 16, ADDR64, LE descriptors)
- NCFGR 0x00560542 (FD, GBE, 128-bit DBW, MDC/96, DRFCS)
- TBQPH/RBQPH offsets
- BCM54213PE delay programming (AUXCTL misc RGMII_SKEW_EN, shadow 0x1c GTXCLK_EN)
- DMA offset 0x10_0000_0000 (pcie2 dma-ranges and RC_BAR1)
- clk_eth 125 MHz

Linux does not touch RP1 eth_cfg.

## Diagnostics added (macb)
- /proc/net/nic gets:
  - the MAC's own counters (clear-on-read, accumulated): `hw_tx_frames`, `hw_rx_frames`, the FCS, symbol, alignment, undersize, jabber, resource and overrun counters, and `hw_tx_underruns`
  - ISR bits seen in polled mode: `isr_tcomp`, `isr_rcomp`, `isr_txubr`, `isr_rxubr`, `isr_hresp`, and so on. Before this, `hresp`/`rx_used_bit_read` were only counted inside the ISR, so they were always 0 when polled.
  - `reg_ncr`, `reg_ncfgr`, `reg_nsr`, `reg_usrio`, `reg_dmacfg`, `reg_tsr`, `reg_rsr`, `reg_tbqp`, `reg_rbqp`, `reg_tbqph`, `reg_rbqph`, `reg_amp`, `reg_imr`
  - `phy_mode`
  - the PHY's RX error / false carrier / not-OK counters and the latched delay bits
  - RP1 `eth_cfg` words 0x00..0x1c
- One `[macb] diag +10s: ...` line 10 s after link-up.
- RX-delay retry: if a 6 s window shows only line errors and no good frame, the PHY RX skew is flipped. It retries at most 3 times and is off when `macb.phy=` is given.
- `macb.phy=rgmii|rgmii-id|rgmii-rxid|rgmii-txid` overrides the PHY delays for A/B tests.

## Evidence
- Host tests: brcmstb-tests 18/18 (new `sub_dword_command_write_sets_bus_master_and_spares_status`); macb-tests 9/9 (new stats, verdict and ISR test)
- check-rpi5.sh clean on rpi5 and raspi4b
- aarch64 release regress (QEMU virt): PASS, 0 boots failed

## Expected next Pi boot
    [rp1] command=0x6 (mem+bus master)
    [macb] link up 1000 Mbit/s full duplex (NCFGR 0x00560542)
    [macb] diag +10s: hw tx N>0 ... | hw rx N>0 fcs 0 ... | isr tcomp >0 ... hresp 0 ...
then eth0 gets a DHCP lease 192.168.2.x, and xHCI enumerates the gamepad (no "command type 9 timed out").

If rx stays 0:
- `fcs`/`sym` > 0: RGMII RX timing. Watch for the auto-retry line, or try `macb.phy=rgmii-txid`.
- `hresp` > 0: the inbound window.
- `hw tx` 0 with `TSR` showing TXGO: no TX clock (eth_cfg).
