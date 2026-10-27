# Lane pigio — BCM2712 GPIO / pinctrl / l2-intc + OTP (2026-10-27)

Status: DELIVERED (host-tested + rpi5/raspi4b compile + aarch64 virt regress; no Pi boot yet).

## What
- `drivers/src/bcm2712_gpio.rs`: brcmstb `gio` (0x7d508500) and `gio_aon` (0x7d517c00), bank
  widths by stepping (C0 gio 32+22 / aon 17+6; D0 gio 32+4 / aon 15+6); `pinctrl` + `pinctrl_aon`
  with the C0 and D0 tables generated from Linux `pinctrl-bcm2712.c` (rpi-6.6.y) by
  `scripts/gen-bcm2712-pinctrl.py` into `drivers/src/bcm2712_pinctrl_tables.rs`; brcmstb l2
  intc (edge / bcm7271 level / bcm2711 level layouts) chained on the GIC.
- Stepping always from `bcm2712_id` (chip id, DT fallback); unknown stepping = pin mux refused.
- Kernel API (rpi5): `gpio_set_output/set_input/get/is_output(Chip, line)`,
  `pin_set_function/pin_set_pull/pin_get_fsel/pin_get_function/pin_apply(Block, ..)`, groups
  `GROUP_SDIO2_WL_ON`, `GROUP_UARTA`, `GROUP_BT_SHUTDOWN`, consts `WL_ON`=28, `BT_ON`=29,
  `gpio_request_irq(line, Trigger, handler, ctx)` (gio -> main_irq hwirq 0 -> SPI 244 = INTID 276),
  `l2_request(L2Id, hwirq, handler, ctx)`. Interrupt plumbing is lazy: no l2/gio IRQ register is
  touched until the first request.
- l2 consumers in the D0 tree: main_irq (SPI 244) <- gio; bsc_irq (SPI 242) <- HDMI ddc0/ddc1;
  cpu_l2_irq (SPI 238, edge) none; aon_intr / main_aon_irq / l2@7d517000 disabled. The BT uart
  (uarta, SPI 276 = INTID 308) is a direct GIC line, not behind an l2.
- sdio_host: WL_REG_ON and the sdio2 mux now go through the driver. Same writes in the same order:
  host test `apply_sdio2_d0_writes_once` replays the old algorithm and compares the write log;
  `sdio2_group_matches_previous_rows` checks C0+D0 against the old hand-written rows.
- pibt-uart landed first (b73ce1bf): its `gio_drive`/`GIO_PHYS`/`PINCTRL_PHYS` imports from
  sdio_host are folded onto the new API (`bt/boot.rs`: `bt_on()`, `pinmux()`; same log format).
- `drivers/src/bcm2712_otp.rs`: GET_BOARD_SERIAL (0x00010004) + GET_BOARD_MAC_ADDRESS (0x00010003)
  via `rpi_misc::fw_tag` after the mailbox is mapped. `macb` order: DTB -> otp -> SA1 -> generated.
  brcmf: DTB fmac `local-mac-address` -> otp eth MAC + 1 (low 24 bits, OUI kept) -> nvram.
- Boot (init.rs, after rpi_misc::boot): `bcm2712_gpio::boot(serror_checkpoint)` (read-only; an
  SError marks the driver dead and every call returns Unavailable), then `bcm2712_otp::boot()`.

## Expected Pi lines
- `[gpio] stepping=D0(Hw) gio=36 gio_aon=21 wl_on=out:0|in:0 bt_on=... uarta24-27=uart0,uart0,uart0,uart0 aon_gpio0=... l2=main_irq:spi244,bsc_irq:spi242,cpu_l2:spi238 ok`
- `[otp] serial=<16 hex> mac=2c:cf:67:xx:xx:xx wifi=<mac+1>`
- `[sdio] ... pinmux=D0 sd2-fsel-before=... writes=N` unchanged; `[brcmf] mac ... dtb` (or `otp` if the DTB lacks it).
- `[bt] uarta D0 mux24-27=4444 gpio29-fsel=0 ...` unchanged format.

## Tests
- `drivers/bcm2712-tests` 11/11 (`cargo test --target aarch64-apple-darwin`), sdio-tests 17/17, bt-tests 15/15.
- `scripts/check-rpi5.sh` rpi5 + raspi4b clean.

## Open
- GPIO IRQ path untested on hardware (no consumer yet). OTP not wired to /etc/machine-id.
