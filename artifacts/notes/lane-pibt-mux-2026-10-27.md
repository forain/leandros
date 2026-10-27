# lane/pibt-mux — 2026-10-27 — FIXED (pending Pi boot)

Real Pi 5 (D0, main 4b975f56): `[bt] uarta D0 mux24-27=0000 ... lsr=0x0` then
`[bt] fail stage=reset err=Tx(3075)`. Root cause: the D0 firmware leaves GPIO 24-27
as plain GPIOs (the old comment assumed `skip-init` meant "already muxed"). With MCR.AFE
the UART's CTS input was unconnected/deasserted, so the first HCI Reset sat in the TX FIFO
(timed out waiting for the event) and the retry found THRE clear -> `Tx(0x0c03)`.
`bt_on=out:0` in the `[gpio]` line is only the pre-bring-up state (that probe is read-only
and runs before the bt thread, which already power-cycled BT_ON).

Fix (c8eb883a):
- `bcm2712_gpio::bt_pins_prepare`: uarta `default` pinctrl state as Linux applies it —
  `bt_shutdown_pins` (29 gpio) + BT_ON driven low first, then `uarta_24_pins`
  (24 RTS / 26 TXD uart0 no bias, 25 CTS / 27 RXD uart0 pull-up). D0 uart0 = fsel 4 on all four
  (generated Linux bcm2712 D0 table). Idempotent.
- Power cycle: BT_ON low `BT_OFF_MS`=50 ms, high, settle `BT_ON_SETTLE_MS`=200 ms (was 10/100).
- `[bt] uarta` line shows pre->post mux, pulls, BT_ON; fail line adds `lsr=`/`msr=`
  (THRE 0x20 clear + MSR.CTS 0x10 clear = still flow-control held).
- Pinmux error -> one `[bt] fail stage=pinmux ...` line, BT_ON low, boot continues.

Evidence: bcm2712-tests 12/12 (new `bt_pins_prepare_from_d0_boot_state` fails with the
uart mux removed), bt-tests 23/23, check-rpi5.sh rpi5 + raspi4b clean, aarch64 regress PASS (nettag2test known flake, 1/1 on rerun).

Expected next Pi boot:
    [bt] uarta D0 mux24-27=0000->4444 pull24-27=0202 gpio29-fsel=0 bt_on=out:0 lsr=0x0 baud=3000000
    [bt] hci ver=... manuf=305 fw=patched(...) baud=3000000 bdaddr=... ok
If it still fails: `[bt] fail stage=reset err=Tx(3075) lsr_err=0 lsr=0x.. msr=0x..` —
msr bit 0x10 clear => CTS still low (check pad pull/function readback); lsr stuck 0x0 even
after init => the uarta block itself is clock-gated/held in reset (next suspect).
