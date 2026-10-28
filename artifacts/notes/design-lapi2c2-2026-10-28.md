# lapi2c2: why the CPD0 HID descriptor read fails, and the phase-2 plan (2026-10-28)

Base: main e590b436 (lapi2c phase 1). Evidence: laptop dmesg (lapverify-out/dmesg*.txt) and the
decompiled laptop DSDT (scratchpad/lapdsdt/DSDT.dsl, `Device (CPD0)` at line ~24055, `HIDD` at 21275).

## 1. Root cause: `CPD0._INI` never runs, so `_DSM` returns `HID2 = 0`

The DSDT's CPD0:
```
Name (HID2, Zero)                     // descriptor register, 0 until _INI
CreateWordField (SBFG, 0x17, INT1)    // GPIO pin in the _CRS template, 0 until _INI
Method (_INI) { ... INT1 = GNUM (GPDI); INT2 = INUM (GPDI); If (SDM1 == 0) { SHPO (GPDI, One) }
                HID2 = 0x20; BADR = 0x2C; SPED = 400000 }
Method (_DSM, 4) { If (Arg0 == HIDG) { Return (HIDD (Arg0..Arg3, HID2)) } ... }
HIDD: If (Arg2 == One) { Return (Arg4) }   // returns HID2 as an Integer
```
- `desc=0x0` in our log is an **Integer 0**, not the fallback `Buffer{0}`: `V::as_int()` returns `None`
  for a buffer (drivers/src/acpi_power/aml.rs:75), and `hid_desc_reg` would then be `None`, not
  `Some(0)`. So the GUID compare worked and `_DSM` returned `HID2`, still at its load-time value 0.
- The second symptom has the same cause: `gpio=\_SB.PCI0.GPI0:pin0`. Pin 0 is the static template value
  that `_INI` overwrites with `GNUM(GPDI)`.
- Our namespace init runs `_INI` only for `\_SB`, `\_SB.PCI0`, the EC, batteries and AC
  (drivers/src/acpi_power/power.rs:278-295). Linux (acpi_ns_initialize_devices) runs `_INI` on
  every device whose `_STA` reports it present. drivers/src/i2c_acpi.rs:154-173 evaluates `_STA`,
  `_PS0`, `_CRS` and `_DSM` of the slave, but never `_INI`.

**The bus side matches.** IC_TX_ABRT_SOURCE `0x1e000008`: bit 3 = `ABRT_TXDATA_NOACK`, and bit 0
(`ABRT_7B_ADDR_NOACK`) is clear. The pad at 0x2c **ACKed its address** and NACKed the first
register byte (0x00). Bits 31:23 (`TX_FLUSH_CNT`) = 0x3C. That is only the count of flushed FIFO
entries, and it carries no diagnostic value. SCL timing is ruled out because the address phase
completes. The controller has no FMCN because it is inside `If (USTP)` (DSDT ~23270), so the
defaults 0x48/0xa0/0x1e are used, which is what we log. The power-up path is not the cause either:
CPD0 has no `_PS0` or `_PR0`, and `_STA` returns a constant 0x0F.

## 2. Intended diff (fast-worker)

drivers/src/i2c_acpi.rs, in `scan()`, between line 157 (`let live = ...`) and the `_PS0` block:
```rust
if live && ns.child(n, "_INI").is_some() {
    s.ini_ran = ns.eval_child(h, n, "_INI", &[]).is_some();   // new bool in I2cSlave
}
```
- Run it only for live slaves, and before `_CRS` and `_DSM`. Both read fields that `_INI` writes:
  INT1 in `_CRS` (SBFG) and HID2 in `_DSM`.
- Also run the controller's `_INI`, if present, once per controller. Reuse the existing
  `ctrl_done` guard pattern.
- `_DSM` hardening at line 173: accept only `V::Int`. Log `dsm=<kind>` when the value is not an
  integer. `describe()` should also print `ini=1`.
- Do not widen this into a global "every device `_INI`" walk in this lane. Running GFX0, USB and
  similar `_INI`s at boot is a separate, riskier change. Add it to TODO as a follow-up at
  power.rs:290-295.

drivers/src/i2c_hid.rs:79-90. **Make the default `i2c_hid=probe` side-effect-free.** Today it does
SET_POWER ON + RESET. Once the descriptor read succeeds, that RESET will run for the first time. On
a dual-interface Synaptics part, RESET can switch the pad from PS/2 to I2C mode and kill the working
PS/2 pointer.
- `probe`: read the HID descriptor and the report descriptor only. No SET_POWER and no RESET.
- `probe+reset` (new, opt-in): the current sequence, followed by a check that PS/2 packets still
  arrive (see the test below).

## 3. Probe-only test on the laptop

1. Build release.
2. Deploy with the laptop-deploy-kernel skill (default cmdline, i.e. `i2c_hid=probe`), boot once,
   and grep dmesg for:
   - `i2c-acpi: \_SB_.PCI0.I2C1.CPD0 hid=SYNA3071 ... addr=0x2c speed=400000 gpio=\_SB.PCI0.GPI0:pin<N != 0> ... desc=0x20`
   - `i2c-hid: CPD0 desc 1e 00 00 01 ...`
   - `i2c-hid: CPD0 vid=06cb pid=<xxxx> ver=... rd_len=<several hundred> in=... cmd=...`
   - `i2c-hid: CPD0 report desc <rd_len> B crc=... <summary with contact_count / maxc_feature>`
3. Record the pid, rd_len and crc in the lane note. Synaptics PTP report descriptors are typically
   500-800 B. A value of 0 or more than 4096 is a failure.
4. Confirm the pad, and the stick if this unit has one, still move the cursor over PS/2. The
   `psmouse` packet counter or debug log must still increase after the probe.
5. If the report descriptor read NACKs without a reset, stop. Report back before trying
   `probe+reset`.
6. `probe+reset` is a second, separate boot. Its expected lines are `reset write=Ok(()) ack=true`.
   The new fact that boot must record is whether PS/2 packets continue afterwards (yes or no).
7. Then run regress on aarch64 only (lane rule). This code is x86-only, but the build must stay
   green on aarch64.

## 4. Phase 2: I2C-HID PTP → evdev MT

**2a. Parsing** (drivers/src/hid_desc.rs). Extend the parser to cover the following.
- Per-finger collections: Tip Switch 0x42, Confidence 0x47, Contact ID 0x51, X and Y, with
  logical and physical extents and unit exponent, giving resolution in units/mm.
- Contact Count 0x54, Scan Time 0x56, and Button 1 (clickpad).
- Feature reports: Contact Count Maximum 0x55, Pad Type 0x59, Input Mode 0x52 in the
  Configuration collection 0x0E, and Surface/Button Switch 0x57/0x58.
- The **mouse collection** (Generic Desktop 0x02). It carries pad output in non-PTP mode and the
  pass-through pointing stick if this unit has one. Route its reports to
  `rel_pointer::report`, so the stick and buttons survive the switch.
- Host tests: add a fixture with the real descriptor bytes captured in section 3.

**2b. Device setup** (`i2c_hid=1`):
1. SET_POWER ON, then RESET and wait for the ack.
2. SET_REPORT(feature, Input Mode) = 3 (touchpad). Follow Linux hid-multitouch
   `mt_set_modes`: with `MT_INPUTMODE_TOUCHPAD` and surface+button switch = 1.
3. GET_REPORT reads the value back. It must be 3.
4. Input arrives from the GPIO line `_CRS` now reports (active-low level). Phase 2 reads the
   GPI0 pad RX state and reads the input register only while the line is asserted. This replaces
   blind polling of `listen_once`. A real GPIO IRQ is a later step.

**2c. evdev node** (servers/evdev/src/lib.rs). Add `DEVKIND_TOUCHPAD`, hot-plugged via
`register_input_device(kind, "SYNA3071:00 06CB:<pid> Touchpad", BUS_I2C=0x18, 0x06cb, pid)`.
- EV_KEY: BTN_LEFT, BTN_TOUCH, BTN_TOOL_FINGER/DOUBLETAP/TRIPLETAP/QUADTAP/QUINTTAP.
- EV_ABS:
  - ABS_X and ABS_Y as single-touch emulation.
  - ABS_MT_SLOT with range 0..maxc-1.
  - ABS_MT_TRACKING_ID with range 0..65535.
  - ABS_MT_POSITION_X and ABS_MT_POSITION_Y, carrying the HID resolution (libinput needs it).
  - ABS_MT_TOOL_TYPE is optional.
- EVIOCGPROP: INPUT_PROP_POINTER, plus INPUT_PROP_BUTTONPAD when Pad Type = 0 (clickpad) or when
  there is only Button 1.
- EVIOCGABS(code): return input_absinfo for every advertised axis, ABS_MT_SLOT included.
- **EVIOCGMTSLOTS** (`_IOC(READ,'E',0x0a,len)`). The user buffer is `{u32 code; i32 values[n]}`.
  Fill `n = min((len-4)/4, slots)` from the per-slot state table. libinput calls it at device add.
- Frame emission follows protocol B. For each changed contact emit:
  - ABS_MT_SLOT
  - ABS_MT_TRACKING_ID, set to -1 on lift
  - MT_POSITION_X and MT_POSITION_Y
  
  Then emit BTN_TOUCH and BTN_TOOL_*, ABS_X/Y of the oldest contact, BTN_LEFT, and SYN_REPORT.
  Handle Contact Count > 1 hybrid mode, where a frame spans several reports.
- Drop contacts with Confidence = 0 (palm), as hid-multitouch does.
- libudev shim: the node needs ID_INPUT=1 and ID_INPUT_TOUCHPAD=1, or libinput treats it as a
  mouse.

**2d. Safe switch-over.** The new knob is `touchpad=ps2|i2c|auto`, default **ps2**.
- **Never disable psmouse.** It stays bound in every mode. The pad reports on only one interface,
  so if I2C fails or the pad falls back, PS/2 output resumes and keeps flowing into rel_pointer
  with no extra work. A hard watchdog only marks the I2C node dead.
- **Commit order for `i2c`:**
  1. The full probe succeeds: descriptor, report descriptor, and a parsed PTP collection with at
     least one finger X/Y.
  2. Register the evdev node.
  3. RESET plus Input Mode = 3, verified by GET_REPORT.
  
  Any failure before step 3 leaves the pad untouched, still in PS/2 mode.
- **Failure after step 3:** send SET_POWER SLEEP to the I2C device and remove the evdev node.
  Then issue a PS/2 re-init: psmouse reset 0xFF and re-enable Synaptics absolute/W mode, so the pad
  returns to PS/2.
- **Runtime watchdog:** 20 consecutive I2C transfer errors, or RESET not acked within 1 s, triggers
  the same fallback. Log one line: `touchpad: i2c failed (<why>), back to ps2`.
- **Must be verified on the laptop before `auto` exists:** whether PS/2 re-init actually revives
  pad output after an I2C RESET. Test it with `touchpad=i2c` plus a debug knob that forces the
  fallback.
- `auto` becomes the default only after 3 clean laptop boots. A USB mouse and the previous-good
  Limine entry remain the last-resort escape hatches.
