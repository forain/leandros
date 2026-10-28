# lane/lapptp (2026-10-28): touchpad phase 2b, I2C-HID Precision Touchpad driver (opt-in)

PARTIAL: set-up, node, parser and fallback work; GET_REPORT read-back of Input Mode NAKs on the real pad, so
`touchpad=i2c` currently always falls back to PS/2 unless the debug knob `touchpad.verify=0` is given.

- drivers/src/hid_ptp.rs (new, host-tested in drivers/hid-desc-tests, 12 tests): PTP report decode (tip, confidence,
  contact id, X/Y, contact count, button), hybrid-frame Assembler, protocol-B `Mt` event synthesis (SLOT/TRACKING_ID/POS,
  BTN_TOUCH, BTN_TOOL_*, ABS_X/Y of oldest, BTN_LEFT, palm = confidence 0 dropped), mouse collection decode, whole
  configuration feature report (Input Mode 3 + surface/button switch 1).
- drivers/src/i2c_hid.rs: knob `touchpad=ps2|i2c|auto` (default ps2, auto = ps2 for now). `i2c`: parse ok -> register
  DEVKIND_TOUCHPAD node "CPD0 06CB:82F1 Touchpad" -> SET_POWER ON, RESET (ack), SET_REPORT feature id 4, GET_REPORT
  verify (retried 30x10 ms). Any failure after the node exists: SET_POWER SLEEP, unregister node, kernel hook re-runs
  x86 `psmouse::init`; log `touchpad: i2c failed (<why>), back to ps2`. Runtime: 20 consecutive transfer errors -> same.
  Polling every 8 ms (GPIO pad level not wired; drains up to 4 reports per poll). Mouse collection -> rel_pointer SRC_STICK.
  Debug knobs: `touchpad.fail=1` (forced fallback 3 s after set-up), `touchpad.verify=0` (skip read-back).
- kernel/src/lappower.rs: registers the PS/2 hook, uses `poll_interval_ns()` (10 ms probe, 8 ms active).

Laptop evidence (trials on main 0fe465cd+):
- `touchpad=i2c`: node registered, RESET acked, SET_REPORT accepted, GET_REPORT write NAKed (TX_ABRT 0x6000008 =
  TXDATA_NOACK) on every retry, also with the full 2-field report -> fallback ran: node removed, psmouse re-init printed a
  fresh Synaptics identify and "mode set 0xc5", so PS/2 re-init after an I2C RESET revives the PS/2 path (event nodes
  event0-2 as before; reports verified only by log, nobody touched the pad).
- `touchpad=i2c touchpad.verify=0`: "i2c active, polling every 8 ms", touchpad node present (event2, power button moved
  to event3), keyboard and PS/2 nodes present, zero input reports while idle, greetd+cosmic-comp up, Wi-Fi link up.
- default knobs: no touchpad line, no node, greeter+cosmic-comp up, Wi-Fi up, DHCP.
- Open: why GET_REPORT NAKs (try other lengths/delays, or read the input queue first); real finger/scroll/tap unverified.
- Caution: the laptop's `good` entry currently carries `touchpad=i2c` (an earlier trial was promoted by mistake); it is
  harmless (falls back) but should be removed when good is redeployed.

## USER test steps (laptop, PS/2 stays the default)
1. `LEANDROS_REMOTE=172.16.149.179 python3 scripts/remote.py deploy-kernel --reboot --append 'touchpad=i2c touchpad.verify=0'`
2. Log in. Check `ls /dev/input` shows an extra node and the console log has `touchpad: i2c active (CPD0)`.
3. Move one finger (pointer should move), two-finger scroll, tap-to-click, physical click. The trackstick should still move.
4. If the pointer is dead or erratic: plug in a USB mouse / use the keyboard and reboot; the trial falls back to `good`
   (no promote). Without `touchpad.verify=0` the kernel falls back to PS/2 by itself. Plain `touchpad=i2c` is expected to fall
   back with `GET_REPORT mode ...` today.
