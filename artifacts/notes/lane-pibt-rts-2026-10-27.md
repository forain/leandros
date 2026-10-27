# lane/pibt-rts — 2026-10-27 — FIX + DIAGNOSTICS (pending Pi boot)

Real Pi 5 (D0, main 910d71e7, kexec boot):
`[bt] fail stage=version err=NoAnswer(4097) baud=115200 rx=0[-] resets=1 vers=2 lsr_err=0 lsr=0x60 msr=0x11`

UART type verified: DT `uarta: serial@7d50c000` is `brcm,bcm7271-uart` (reg-shift 2, io-width 4,
96 MHz, uart-has-rtscts, auto-flow-control) = 8250 PORT_BCM7271 (32-byte FIFOs, FCR R_TRIG_01 =
8-byte RX trigger, UART_CAP_AFE). It is NOT a PL011, so the 16550 LSR/MSR readings are genuine.

Analysis:
- The Reset answer was real controller output (Command Complete for 0x0c03, status 0; an echo would be
  an H4 command packet, not an event). So pins, rate (96 MHz / 52) and TX/RX all work.
- Version cmd left the UART (lsr TEMT=1, CTS asserted now), no RX byte pending, no line errors.
- Old diag showed only the *second* version window; the first attempt's bytes were never reported
  (the parser was reset between attempts). A stall after exactly 8 bytes (auto RTS stuck deasserted at
  the 8-byte trigger: Reset's 7-byte answer is below it, version's 15 bytes are not) leaves exactly
  this picture: 8 bytes drained, rest held by the controller, attempt 2 sees nothing.
- Remaining alternative: controller genuinely silent after Reset (not flow). The new lines separate them.

Fix (bb780ed2): on the first unanswered Read Local Version the bring-up keeps that window + modem
lines, switches the UART to `Flow::Soft` (MCR.AFE off; RTS asserted only while recv polls, dropped
before any sleep; TX gated on MSR.CTS), drains what the controller releases, Reset (resync) and asks
again. Patch download, 3 Mbaud switch, BD_ADDR and the /dev/hci0 pump continue in that mode.
Diagnostics: `flow=auto|soft`, `mcr/msr`, `rts_pad`/`cts_pad` (gio 24/25 DATA, low = asserted), `dcts`
count, and `v1=rx:N[hex]` = the first (auto-RTS) version window.

Evidence: bt-tests 28/28 (new: auto_rts_stall_falls_back_to_soft_flow — fake that holds an answer
past 8 bytes until soft flow; healthy_controller_stays_in_auto_flow; silent case updated). With the
fallback disabled the stall test fails. check-rpi5.sh rpi5 + raspi4b clean (no new warnings);
aarch64 regress PASS (0 boot failures).

Expected next Pi boot:
    [bt] uarta D0 mux24-27=4444->4444 pull24-27=0202 gpio29-fsel=0 bt_on=out:0 lsr=0x60 baud=3000000
    [bt] hci ver=... manuf=305 fw=patched(...) baud=3000000 ... resets=2 vers=2 flow=soft ...
    [bt] flow fallback: auto-RTS version window rx=8[04 0e 0c 01 01 10 00 ..] mcr=0x23 msr=.. rts_pad=1 ...
    [bt] lines mcr=0x1|0x3 ...
Reading a failure: `v1=rx:0[-] v1_... rts_pad=0` = RTS was asserted and the controller still said
nothing -> not flow control (next: controller state after Reset, e.g. send 0xfc79 like btbcm, LPO clock).
`v1=rx:8..` + `rts_pad=1` = auto RTS stall confirmed. If auto mode works, `flow=auto vers=1`.
