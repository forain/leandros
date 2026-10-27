# lane/pibt-ver — 2026-10-27 — FIXED (pending Pi boot)

Real Pi 5 (D0, main c6f5fc63, after pibt-mux):
`[bt] fail stage=version err=NoAnswer(4097) lsr_err=0 lsr=0x60 msr=0x11` —
HCI Reset (0x0c03) answered at 115200, the very next Read Local Version (0x1001,
still 115200: the 3 Mbaud switch comes after the patch download) got nothing.

Root cause (by comparison with Linux btbcm.c): `btbcm_reset()` sleeps 100 ms after
every HCI Reset ("delay for module to complete reset process"); the bring-up sent the
next command immediately. The BCM4345C0 re-initialises after answering Reset and drops
commands inside that window. MSR.DCTS (0x01) set is consistent with the controller
toggling its RTS (our CTS) while re-initialising. Ruled out: rate (failing stage is
before any baud change; Reset at the same rate worked; clock 96 MHz/div 52 proven by the
Reset answer); RX overrun (lsr_err=0, OE counted); stage order (matches hci_bcm:
reset → version → minidriver/patch → 250 ms → reset → 0xfc18 → host rate).

Fix (f556203f): `RESET_SETTLE_MS`=100 after the ROM Reset and after the patched Reset;
first Read Local Version retried once (after another settle) on NoAnswer. Failure line
now shows `baud=` (host rate in force), `rx=N[hex]` (bytes the failing command's wait got,
`-` = none), `resets=`, `vers=`; success line adds `resets=`/`vers=`.

Evidence: bt-tests 26/26 (new: reset_settle_before_next_command — controller that drops
commands until a >=100 ms wait; rate_mismatch_failure_reports_baud_and_raw_bytes;
silent_after_reset_is_version_no_answer_with_empty_rx). Without the settle sleep 3 fail.
check-rpi5.sh rpi5 + raspi4b clean; aarch64 regress PASS (all suites, 0 boot failures).

Expected next Pi boot:
    [bt] uarta D0 mux24-27=0000->4444 pull24-27=0202 gpio29-fsel=0 bt_on=out:0 lsr=0x0 baud=3000000
    [bt] hci ver=... manuf=305 fw=patched(...) baud=3000000 bdaddr=... ms resets=1 vers=1 ...
If it still fails: `[bt] fail stage=version err=NoAnswer(4097) baud=115200 rx=0[-] resets=1 vers=2 ...`
= controller silent after Reset even with settle (next suspect: auto-RTS on uarta,
host RTS not reasserting after the 8-byte trigger — Reset answer is 7 bytes, version 15);
`rx=N[..]` non-empty = bytes arrive but don't parse (paste them).
