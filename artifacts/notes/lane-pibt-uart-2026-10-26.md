# lane/pibt-uart 2026-10-26 — Pi 5 Bluetooth: uarta + BT_ON + H4 + BCM4345C0 patchram (lanes 7+8 of pisound-design)

Status: DELIVERED (code + host tests; unproven on hardware until the next Pi boot). /dev/hci0: HANDOFF.

## What
- `drivers/src/bt/h4.rs` H4 encoder + streaming reassembler (resyncs on garbage), event / Read Local
  Version / Read BD_ADDR parsers, BdAddr (placeholder detection 43:45:c0:00:1f:ac).
- `drivers/src/bt/hcd.rs` `.hcd` record walker + validate (all vendor opcodes, exactly one Launch RAM, last).
- `drivers/src/bt/bringup.rs` btbcm/hci_bcm order over a `Transport` trait: Reset (1 retry) → Read Version →
  0xfc2e, 50 ms, records, 250 ms, Reset → 0xfc18 3 Mbaud + host switch → Read Version, Read BD_ADDR →
  0xfc01 from the live DTB `local-bd-address` if placeholder. Every command bounded (1 s, 64 KiB rx).
- `drivers/src/bt/boot.rs` (rpi5): uarta @0x10_7d50_c000. **Correction to the brief: it is NOT a PL011** —
  DT says `brcm,bcm7271-uart` (8250 family): 16550 regs, reg-shift 2, 96 MHz, divisor 52 @115200 / 2 @3M,
  MCR.AFE auto RTS/CTS. RX polled (AFE holds the sender off, so no #101-style loss); IRQ (SPI 276) unused.
  BT_ON gio 29 via `sdio_host::gio_drive` (WL_REG_ON's accessor, now under a lock); GPIO29 fsel forced to 0,
  GPIO 24-27 mux only read and logged. serror_checkpoint before/after the first MMIO and after each phase.
  Runs on kthread `bt` after root mount (120 s cap). Failure: one `[bt] fail stage=…` line, BT_ON low.
  Knobs: `bt=0`, `bt.baud=115200` (stay at init rate), `bt.fw=0` (ROM only).
- `ports/bluez-firmware/fetch.sh`: BCM4345C0.hcd @ cdf61dc6, sha256 51c45e77…6dea42c, 63806 B;
  mkfs-f2fs-populated.py stages it to /lib/firmware/brcm/ (aarch64), prepare-rpi5-sdcard.sh fetches it.

## Evidence
- drivers/bt-tests: 15/15 (real .hcd: 323 records, 62837 payload bytes; full transcript vs scripted
  controller with vendor-event noise; lost reset retry; silent controller; record status error; bad image
  never touches the controller; baud mismatch = bounded NoAnswer; fdt local-bd-address). Mutation check:
  removing the launch-last check / the 250 ms wait fails 2 tests.
- check-rpi5.sh rpi5 + raspi4b clean; aarch64 virt build + boot + short suite (see merge).

## Expected on the next Pi boot (card must be re-prepped or the .hcd pushed to /lib/firmware/brcm/)
    [bt] uarta D0 mux24-27=xxxx gpio29-fsel=0 lsr=0x60 baud=3000000
    [bt] hci ver=9 rev=0x… lmp_ver=9 lmp_subver=0x6119 rom_subver=0x…. manuf=… fw=patched(323 records, 62837 bytes) baud=3000000 bdaddr=d8:3a:dd:..:..:.. (set-from-dt|controller) … ms
  or `[bt] fail stage=<reset|version|minidriver|patch|reset-patched|baud|version-final|bdaddr> err=…`.
  `fail stage=reset err=NoAnswer(0xc03)` = no UART link (mux/flow control/BT_ON); then try `bt.baud=115200`.

## Open
- /dev/hci0 (BT_SEAM.md section A): handoff $SP/handoff/pibt-hci0.md (budget).
- IRQ-driven RX; mailbox GET_BOARD_MAC+1 fallback for BD_ADDR (no mailbox helper in tree).
