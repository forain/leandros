# design lapptp2 (2026-10-28): why GET_REPORT(Input Mode) NAKs on CPD0, and the fix

Base: main 8e4e1893. Files: drivers/src/i2c_hid.rs, drivers/src/dw_i2c.rs (no change needed), drivers/src/hid_ptp.rs (no change).

## 1. Byte decode: our GET_REPORT is spec-correct

`get_feature` (i2c_hid.rs:98-107) with cmd=0x22, data=0x23, id=4, type Feature=3, opcode 2:

    W: 22 00 | 34 02 | 23 00        Sr  R: len_lo len_hi 04 <plen bytes>  P

Linux `i2c_hid_get_report` sends exactly this: cmd reg LE16, `i2c_hid_encode_command` (type<<4|id, opcode;
id>=15 -> type<<4|0xF, opcode, id), data reg LE16, then a second I2C_M_RD message of `len+2` (repeated start).
`cmd_hdr` (91-95) packs the nibble and the escape identically; SET_POWER (0x00/0x01,0x08), RESET (0x00,0x01) and
`set_feature` (length word = 2+1+plen, id, payload) also match Linux. dw_i2c.rs:86-95: no STOP on the write phase,
RESTART (bit 10) on the first read, STOP (bit 9) on the last; IC_CON 0x65 has RESTART_EN. That same path reads the
HID and report descriptors fine. ABRT 0x6000008 = bit 3 TXDATA_NOACK (a data byte of the write phase, not the
address) + TX_FLUSH_CNT 12. So it is not an encoding or DW sequencing bug: the pad's firmware refuses the request.

## 2. Cause (two candidates, one trial separates them)

A. (most likely) GET_REPORT on the Input Mode feature is not supported by this firmware. Windows never reads Input
   Mode back (it only SETs it, and GETs Device Capabilities / the certification blob); Linux hid-multitouch only
   reads it with MT_QUIRK_FORCE_GET_FEATURE. So Synaptics firmware has no reason to answer it and NAKs the command.
   The read-back is our invention (i2c_hid.rs:148-167), not a protocol step.

B. Reset completion is not honoured (i2c_hid.rs:138-145). The "ack" loop starts immediately after RESET and accepts
   the first `[0,0]`, but an idle pad also returns length 0, so "acked" proves nothing. Linux sets
   I2C_HID_RESET_PENDING, waits up to 1 s for the IRQ, and the ISR's plain read of the 0x0000 sentinel clears it
   (I2C_HID_QUIRK_NO_IRQ_AFTER_RESET: msleep(100) instead). We send SET_REPORT possibly while the pad is still
   resetting (so the mode may never have been applied) and never drain the sentinel / pending reports before the
   30 GET retries, which never read the input register in between. A pad holding INT with an unread report can NAK.

Both are fixed by the same diff; the trial log says which one it was (GET of feature 8 succeeds => A).

## 3. Intended diff (i2c_hid.rs only)

1. New helpers:
   - `fn read_input(bus, addr, buf) -> Result<usize,u32>`: `dw_i2c::xfer(bus, addr, &[], buf)` (plain read, as
     Linux `i2c_hid_get_input` / `i2c_master_recv`), returns the LE16 length.
   - `fn drain(bus, addr, max_in) -> (u32 reports, u32 errs)`: up to 8 `read_input(max_in)` until len <= 2.
   dw_i2c::xfer with empty `w` already emits read cmds with no RESTART and STOP on the last (dw_i2c.rs:90-95).
2. setup_pad after RESET (replace 139-145): spin_ms(100) (Linux no-IRQ quirk value), then poll up to 1 s:
   `read_input(max_in)` until Ok with len == 0 (sentinel); then `drain`. Log `reset done in N ms, drained R`.
   (Pass `max_in` into setup_pad; it is parsed in probe_slave:196.) Same 100 ms + drain in the probe+reset path
   (probe_slave:204-210).
3. After SET_REPORT (147): spin_ms(10); `drain`.
4. Verification (replace 148-167):
   - `drain` before every GET retry (inside the retry loop at 150-155), cut to 5 retries x 10 ms.
   - If GET(Input Mode) still fails: GET_REPORT feature `ptp.max_contacts_feature` (id 8, plen from its fields)
     once. Log `GET mode NAK, GET maxc {ok v|NAK e}`. If maxc reads OK => firmware just does not support Input
     Mode GET: accept set-up unverified (`mode_confirmed = false`). If maxc also NAKs => real link/state problem:
     fail -> PS/2 fallback as today.
   - Remove the need for `touchpad.verify=0` (keep the knob; it now just skips step 4).
5. Behavioural confirmation in pad_poll (316-334): add `mode_confirmed: bool, resent: bool` to Pad.
   - First report whose id == ptp.input_id (3): log once `touchpad: PTP reports confirmed`, mode_confirmed = true.
   - A mouse-collection report while !mode_confirmed: log once `touchpad: pad still in mouse mode`; if !resent,
     re-send `set_feature(mode)` once (resent = true). Keep forwarding mouse reports to rel_pointer either way, so
     the pointer is never dead.
6. pad_poll and listen_once (291, 316): use `read_input` (plain read) instead of write(input_reg)+Sr+read. Keep
   the old form behind `i2c_hid.inreg=1` in case this pad prefers it. Length check `len <= 2 || len > n` is right;
   `rep = buf[2..len]` keeps the id byte, which `parse_report(uses_ids)` expects: OK.
7. New knob `touchpad.debug=1`: set dw_i2c::DEBUG only for the setup_pad span, then clear it (i2c.debug=1 would
   hex-dump every 8 ms poll). Logs the exact GET bytes and plen, so TX_FLUSH_CNT can be matched to the NAKed byte.

Input path otherwise correct: 8 ms poll, up to 4 reports per poll, 64-byte reads (max_in=64), hybrid assembler.
GPIO pin 82 (INT, level, active low) is still not wired; polling an idle pad returns len 0 per spec, so it is not
needed for correctness. Wiring it later (read pad level, only read when low) would replace the 100 ms guess in
step 2 with "wait for INT low" and cut idle bus traffic.

## 4. Laptop test plan (lane runs 1-2; the user does 3)

Build release, aarch64 + x86_64 QEMU boot (no LPSS there: expect `no LPSS I2C controllers`), then:

1. Diagnostic trial, no user: `deploy-kernel --reboot --append 'touchpad=i2c touchpad.debug=1'`. Collect dmesg;
   expect, in order: `reset done in N ms, drained R`, SET_REPORT ok, either `Input Mode read back 3` or
   `GET mode NAK, GET maxc ok`, then `touchpad: i2c active`. Record the `dw-i2c ... w[6]=22 00 34 02 23 00 r[..]`
   line(s) and the abort value. If maxc also NAKs: stop, report (candidate B or a deeper state issue).
2. Check `/dev/input` has the extra touchpad node, keyboard and trackstick nodes present, greeter + cosmic-comp up,
   no `20 consecutive transfer errors`. Do not promote.
3. USER finger test (same boot, or redeploy with `--append 'touchpad=i2c'`):
   a. Log in (leandro/leandro). Do not touch the pad until the desktop is up.
   b. Slide one finger slowly across the pad: the pointer must follow smoothly.
   c. Two-finger scroll up/down in a window (e.g. a terminal's scrollback or Firefox).
   d. One-finger tap = left click; two-finger tap = right click (if the compositor enables tap).
   e. Physical click on the pad (press down) = left click.
   f. Rest the palm on the pad edge while typing: the pointer should not jump.
   g. Move the trackstick (red nub) and press its buttons: must still work.
   h. Tell the lane: which of b-g worked. The lane then checks the log for `touchpad: PTP reports confirmed`
      (or `pad still in mouse mode`) and the report counters.
   i. If the pointer is dead or erratic: use the trackstick, USB mouse or keyboard and reboot; the trial entry
      falls back to `good` (nothing promoted).
4. Only after b-e pass: consider `touchpad=auto` -> i2c; also strip the stray `touchpad=i2c` from the `good` entry
   on the next promote (see lane-lapptp note).
