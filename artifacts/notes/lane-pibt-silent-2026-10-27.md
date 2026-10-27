# lane/pibt-silent — 2026-10-27 — FIX (pending Pi boot)

Real Pi 5 (D0, main dd54cdcd, kexec boot): HCI Reset answered twice at 115200, Read Local
Version never (`v1=rx:0[-]`, rts_pad=0 and cts_pad=0 = both asserted: flow control ruled out).

DT check (piirq/rpi5.dts, `bluetooth` under `uarta`): compatible `brcm,bcm43438-bt`,
max-speed 3000000, shutdown-gpios gio 29 active-high, local-bd-address. NO device-wakeup,
host-wakeup/interrupts, clocks (lpo/extclk) or `brcm,requires-autobaud-mode`. So under Linux
(which works with this DT) dev-wake is not driven, LPO is not configured, and autobaud mode
is off: suspects (1) (2) (3) ruled out by the DT itself.

Root cause (by comparison with raspberrypi/linux rpi-6.6.y hci_bcm.c): `brcm,bcm43438-bt`
has `drive_rts_on_open = true`; bcm_open() calls hci_uart_set_flow_control(hu, true)
(flow off + RTS deasserted) BEFORE bcm_gpio_set_power(on) + 100-120 ms, and only then
re-enables flow control. The BCM43xx samples CTS_N (= host RTS) when BT_ON rises; asserted
there it starts in a mode that answers Reset and then ignores commands. Our power cycle ran
with MCR left at DTR|RTS|AFE (by the previous kexec kernel's failed auto-flow run, or by
`uart.init` habits) = RTS asserted at the edge.

Fix (c066c969):
- `bringup::power_cycle`: hold RTS deasserted (MCR=0) -> BT_ON low off_ms -> sample RTS pad ->
  BT_ON high -> 200 ms settle -> RX FIFO flushed, normal (auto) flow control back.
  New Transport hooks `set_power` / `hold_rts_deasserted` (defaults no-op).
- One retry after a failed bring-up (not for a bad .hcd) with BT_ON low `BT_COLD_OFF_MS`=1000
  ms: separates "strap at power-on" from "controller state survived a short pulse" (suspect 4).
- `[bt] hci ...` / `[bt] fail ...` lines end with `attempt=N off=Nms rts_at_on=0|1|?`;
  a failed attempt 1 logs `[bt] retry stage=...` first.

Evidence: bt-tests 30/30 (new: rts_asserted_at_power_on_answers_only_reset — the Pi's exact
log pattern [0c03,1001,0c03,1001] from a fake strapped by RTS-at-power-on;
power_cycle_holds_rts_deasserted_across_bt_on_edge — kexec-left patched/3M controller, full
patch + 3 Mbaud transcript in auto flow; fails with the hold removed). bcm2712-tests 12/12.
check-rpi5.sh rpi5 + raspi4b clean (no bt warnings); aarch64 regress PASS (0 boot failures).

Expected next Pi boot:
    [bt] uarta D0 mux24-27=4444->4444 ... baud=3000000
    [bt] hci ver=... manuf=305 fw=patched(...) baud=3000000 ... resets=1 vers=1 flow=auto ... attempt=1 off=50ms rts_at_on=1
Reading a failure:
- `rts_at_on=0` = the RTS pad did not follow MCR=0 (pad/mux, not the controller): check uarta MCR/mux.
- `retry ... rts_at_on=1` then `hci ... attempt=2 off=1000ms` = a short BT_ON pulse was not a full
  reset (kexec state): raise BT_OFF_MS.
- both attempts `stage=version ... rts_at_on=1 rx=0[-]` = the strap theory is refuted; next:
  send Linux's first command (0xfc18 at 115200, hci_serdev early set_baudrate) before Reset, and
  compare a cold (power-on, non-kexec) boot.
