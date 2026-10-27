# lane/pipcie3 — Pi 5 pcie2 stage-5 stall root-caused and fixed (2026-10-27)

## FIXED (pending Pi verification)

Pi 5 (main 4b975f56, `pcie.rpi5=1`): stages 1–4 fine (rc rev=0x500, status=0x1e080 = RC port bit only,
ctrl=0 = PERST# asserted, link down — the firmware's default `pciex4_reset=1` resets the x4 controller
before the OS starts, so a down link at hand-over is expected). Stage 5 then stalled cpu0 forever, no SError.

Root cause: stage 5 wrote SW_INIT SET (brcmstb-reset id 44 = bank 1 bit 12, DT `resets = <&pcie_rescal>,
<&bcm_reset 44>`, names rescal/bridge — correct) and then did `perst(true)`, a read-modify-write of
MISC_PCIE_CTRL — an RC register access while the bridge reset holds the RC block. The AXI read never completes.
Linux `brcm_pcie_setup()` (drivers/pci/controller/pcie-brcmstb.c) does nothing between
`bridge_sw_init_set(1)` and `bridge_sw_init_set(0)` but `usleep_range(100, 200)`; PERST# is asserted
there only for BCM2711 (whose PERST# lives in RGR1). bcm2712_cfg uses `perst_set_7278` (MISC_PCIE_CTRL
PERSTB), the "bridge" reset_control, and CFG_QUIRK_AVOID_BRIDGE_SHUTDOWN (never re-assert the bridge in turn_off).

Fix: stages 5–7 moved into `brcmstb_hw` behind a `PcieBus` seam:
- 5 bridge-assert: SW_INIT SET + status read + 150 us (reset block only, no RC access);
- 6 bridge-release: SW_INIT CLEAR, poll status (≤1 ms) until released (else FAIL bridge-stuck, RC untouched),
  then one RC REVISION read (0 / all-ones → FAIL rc-dead, no RC writes);
- 7 perst-serdes: first RC writes — PERST# asserted only if the firmware left it released, SerDes IDDQ off.
Each prints a raw-UART result line. `pcie.stop` / `pcie.reuse` unchanged.

Evidence: host tests `drivers/brcmstb-tests` 14/14 (`cargo test --target aarch64-apple-darwin`), new recording mock
flags any RC access while the bridge is asserted; `old_sequence_is_caught_by_the_mock` shows the pipcie2 order trips it.
check-rpi5.sh rpi5 + raspi4b build clean (no brcmstb warnings); aarch64 QEMU default regress PASS (0/1 boots failed).

## Next Pi boot should show
    [pcie] stage 5 bridge-assert
    [pcie] bridge asserted swinit=0x1000
    [pcie] stage 6 bridge-release
    [pcie] bridge released rc rev=0x500
    [pcie] stage 7 perst-serdes
    [pcie] perst+serdes ctrl=0x0 hard_debug=<IDDQ bit 27 clear>
Try `pcie.stop=6` first (bridge pulse + one RC read), then `pcie.stop=7`, then `pcie.stop=12` (link poll).

## Open
- Stages 8–16 reviewed against mainline 2712 setup (MDIO PLL, MISC_CTRL burst=2, endian, class, ASPM L1, gen2,
  CLKREQ) — no discrepancy found; the DT's first dma-range (PCI 0 → CPU 0x1f_0000_0000, 4 MiB) is not mapped
  as an inbound window (not needed for link/enumeration).
