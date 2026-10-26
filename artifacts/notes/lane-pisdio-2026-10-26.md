# lane/pisdio — 2026-10-26 — DELIVERED (hardware unverified)

Pi 5 sdio2 (mmc@1100000, CYW43455) as an SDIO host; `impl SdioBus` in
`drivers/src/brcmf/sdio_host.rs`. Created the frozen brcmf seam (mod/bus/fwil verbatim
from the brief) plus one-line stubs for chip/fw/sdpcm/bcdc/cfg/event/netdev.
`drivers/src/sdhci.rs` (root card on sdio1) is NOT modified.

## Hardware facts used (decoded dtb + Linux sources)
- host 0x10_0110_0000 (0x260), cfg 0x10_0110_0400; SPI 0x112 (unused, polled); clk_emmc2 200 MHz
  (fallback only, CAPABILITIES base clock wins); bus-width 4; non-removable.
- vmmc = wl_on_reg: regulator-fixed, gio (0x10_7d50_8500, brcmstb: DATA +4, IODIR +8, 1=in) pin 28,
  active high, startup-delay 150 ms. Pins: gpio28 "gpio"; gpio30 clk no-pull, 31..35 sd2 pull-up.
- pinctrl 0x10_7d50_4100: c0 and d0 tables differ (Linux pinctrl-bcm2712.c). Stepping from the live
  DTB (`brcm,bcm2712d0-pinctrl` / `c0`); unknown -> mux untouched. c0 sd2 fsel: 30/31/32/34=4, 33/35=3;
  d0: all 1.
- cfg window per sdhci-brcmstb 2712: CTRL bit31 set/bit30 clear (force presence), SD_PIN_SEL=SD.

## Sequence
host quiesce (SWRST_ALL, clock+power off) -> WL_REG_ON low 20 ms -> high -> 150 ms (always, kexec-safe)
-> SWRST_ALL, 3.3 V, 400 kHz -> CMD0, CMD5(0), CMD5(OCR 3.2-3.4 V) until ready (<=1 s) -> CMD3 -> CMD7
-> CCCR rev/caps, CIS MANFID -> 4-bit (+CD disable) -> EHS if SHS -> 50 MHz (verified by CCCR re-read,
falls back to 25 MHz) -> F1 enable, F1 blksz 64 -> chip id (ALP req, SBADDR 0x1000a/b/c = 00/00/18,
CMD53 4 B at F1 0x8000). Whole probe hard-bounded to 2 s (deadline in every wait).
On success the host is parked for brcmf: `drivers::brcmf::sdio_host::take_host()`.
`PROBE_STATE`: 1 ok, 2 chip id bad, 3 init failed, 4 map failed.

## Tests
- `cd drivers/sdio-tests && cargo test --target aarch64-apple-darwin`: 13/13 (split logic incl. 511-block
  cap, fixed/incr addressing, block+byte roundtrip, R5 error flags, timeout recovery, card IRQ, CMD5
  deadline, chip-id window). Split guard shown failing with the cap mutated to 512.
- check-rpi5.sh rpi5 + raspi4b: clean. aarch64 virt regress: see report.

## Next Pi boot should show
OK:   `[sdio] sdio2 base=0x1001100000 ocr=0x...... rca=0x.... cccr=0x.. caps=0x.. funcs=2 manf=0x02d0:0xa9bf bus=4bit 50000kHz hs pinmux=C0 ...`
      `[sdio] chipid=0x....4345 chip=0x4345 rev=6 alp=true ok t=...ms`
FAIL: `[sdio] FAIL stage=<hostver|reset|cmd5|ocr|cmd5-busy|cmd3|cmd7|cccr|cis|busif|busverify|f1-enable|f1-blksz> err=<Timeout|Crc|Io|NoCard> nis/eis=........ pstate=........ pinmux=... t=...ms`
      `[sdio] FAIL stage=chipid ...` / `[sdio] FAIL stage=map`
Open: DMA (PIO only); 1.8 V/UHS not attempted; IRQ line 0x112 unused.
