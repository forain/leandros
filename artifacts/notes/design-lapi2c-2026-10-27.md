# Design: lane lapi2c — LPSS DesignWare I2C + HID-over-I2C (+ MT touchpad) — 2026-10-27

Status: DESIGN ONLY (no code written). Branch `lane/lapi2c` = main c5b3fd62 + this note.
Implementer: fast-worker, own worktree `../leandros-lapi2c` off `lane/lapi2c`, `git merge main` first.

## 1. Findings (what is known, what is not)

- PCI: `00:15.0 8086:9d60` (I2C0), `00:15.1 8086:9d61` (I2C1), no driver (inventory note
  `artifacts/notes/laptop-device-inventory-2026-10-27.md` rows 21, 57-63).
- ACPI children (static walk by lapinv, `_STA` NOT evaluated, so "declared" not "fitted"):
  - `PCI0.I2C0.TPD0` — runtime `_HID` (a Method, HP selects Synaptics/Elan id at run time), `_CID` PNP0C50. Touchpad.
  - `PCI0.I2C0.TPL0` — ELAN25D0 / PNP0C50 touchscreen option (non-touch SKU: expect `_STA`=0).
  - `PCI0.I2C1.TPL1`, `CPD0` — PNP0C50 options.
  - `PCI0.I2C0.UCM1/UCM2` — INT3515 = TI TPS6598x USB-PD (owner lapdisp for alt-mode; we may read status).
  - `PCI0.I2C1.NFC1` NXP1002 (option), `PCI0.I2C1.IMP3` IMPJ0003 (unknown).
- Touchpad today: PS/2 Synaptics on AUX, converted to RELATIVE motion (`drivers/src/psmouse.rs:397 pad_gain`),
  so COSMIC gets no two-finger scroll / gestures. HP decides PS/2 vs I2C via the TPD0 `_HID`/`_STA` methods —
  whether TPD0 is live on THIS unit is UNKNOWN until `_STA` is evaluated on the laptop (step P0 below).
  If TPD0 `_STA`&0xB != 0xB: the pad is PS/2(+SMBus/RMI4) only → report that, and the deliverable becomes
  the I2C bus + TPS6598x status read (section 6).
- Not measured: laptop lock was held by another lane during this design pass; no laptop data collected.
- AML interpreter (`drivers/src/acpi_power`, owner lapdsdt) has NO generic eval API: the namespace lives in
  private `STATE` in `power.rs:14`, accessed only through private `fn with` (`power.rs:326-334`). lapdsdt's
  pending work (uncommitted, read-only check) adds `raw_table()` to mod.rs, nothing for eval.
- No HID report parser in the kernel. `userland/bthid/src/report.rs` (alloc-only) has one but drops item kind
  (Input/Feature), physical extents and link collections — insufficient for PTP. Write a new one.
- evdev: kernel drivers register nodes via `evdev_server::register_input_device` (`servers/evdev/src/lib.rs:2113`)
  and push via `push_event` (`lib.rs:1776`). Caps for non-uinput kinds are hardcoded per kind in `eviocgbit`
  (`lib.rs:1162`), abs info only for tablet (`eviocgabs`, `lib.rs:1235`), props only for tablet (EVIOCGPROP
  0x09, `lib.rs:1495`). `EVIOCGMTSLOTS` (nr 0x0a) is NOT implemented (falls to ENOTTY, `lib.rs:1588`) —
  libevdev calls it on any device with ABS_MT_SLOT, so it MUST be added. uinput ctx (`servers/evdev/src/uinput.rs:12`)
  already stores evbits/key/abs(u64: codes 0..63, ABS_MT_* are 0x2f..0x3d — fits)/absinfo; it lacks props.

## 2. Minimal additive API in acpi_power (coordinate: tell lapdsdt; additive only, ~6 lines)

`drivers/src/acpi_power/power.rs`, right after `fn with` (line 334):
```rust
/// Run `f` on the loaded namespace (for other drivers' _CRS/_DSM/_STA/_PSx).
pub fn with_ns<T>(f: impl FnOnce(&mut Ns, &mut dyn aml::Host) -> T) -> Option<T> {
    with(|s, h| f(&mut s.ns, h))
}
```
`drivers/src/acpi_power/mod.rs`, right after `pub fn init` (line ~66):
```rust
/// Evaluate arbitrary AML under the interpreter lock (task context only).
pub fn with_ns<T>(f: impl FnOnce(&mut aml::Ns, &mut dyn aml::Host) -> T) -> Option<T> { power::with_ns(f) }
```
Everything else (device walk, `_CRS` decode, `_DSM`) lives in the NEW module, using `Ns::find/children/child/
is_real_device/eval_child/path` (`aml.rs:287-416`) and `aml::eisa_id` (`aml.rs:1423`). Copy the `hid_matches`
logic (`power.rs:189-203`) into the new module rather than exporting it.

## 3. New files

### 3a. `drivers/src/i2c_acpi.rs` — find I2C slaves (x86_64 only)
`pub struct I2cSlave { path: String, hid: String, ctrl_adr: u32 /*parent _ADR e.g. 0x0015_0000*/,
 addr: u16, ten_bit: bool, speed_hz: u32, gpio: Option<(String /*ctrl path*/, u16 /*pin*/, u8 /*flags*/)>,
 irq: Option<u32>, hid_desc_reg: Option<u16>, sta: u64 }`
`pub fn scan() -> Vec<I2cSlave>` inside one `acpi_power::with_ns`:
1. for every `is_real_device(n)` whose parent has `_ADR` 0x00150000..=0x00150003 or 0x00190000..=0x00190002
   (SPT-LP I2C0-3 = dev 0x15 fn0-3, I2C4/5 = dev 0x19 fn 2? — just record the parent `_ADR`).
2. `_STA` (absent ⇒ 0xF). Keep every child, log all, use only `sta & 0xB == 0xB`.
3. `_HID` (Int ⇒ EISA decode, Str ⇒ as is), `_CID` (Int/Str/Pkg).
4. `_CRS` buffer walk (small items: tag `(b>>3)&0xF`, len `b&7`; large items: tag `b`, len LE u16 at 1..3):
   - `0x8E` I2cSerialBus: `[5]==1` (I2C); `flags10 = u16[7..9] & 1`; `speed = u32[12..16]`; `addr = u16[16..18]`;
     resource source string at 18.. (the controller path, e.g. `\_SB.PCI0.I2C0`) — prefer it over the parent.
   - `0x8C` GpioInt/GpioIo: `[4]` conn type (0=int); `flags = u16[7..9]` (bit0 edge, bits1-2 polarity);
     `pin_off = u16[14..16]`, `src_off = u16[17..19]`; pin = u16 at pin_off (from descriptor start).
   - `0x89` Extended IRQ: `count=[4]`, first GSI `u32[5..9]`.
   - small `0x79` end tag stops.
5. If PNP0C50/ACPI0C50 in `_HID`/`_CID`: `_DSM(uuid, 1, 1, Package{})`, uuid bytes (ACPI buffer order)
   `f7 f6 df 3c 67 42 55 45 ad 05 b3 0a 3d 89 38 de` → Int = HID descriptor register.
6. Controller: evaluate parent `PSx`/`_PS0` if present (D0); also read `SSCN`/`FMCN` packages under the
   controller node (`{HCNT, LCNT, SDA_HOLD}`) → return as `timing: Option<[u16;3]>` per controller.
Log one line per slave (UART via the same `emit` helpers lapinv/acpi_power use):
`i2c-acpi: \_SB.PCI0.I2C0.TPD0 hid=SYNA3071 cid=PNP0C50 sta=0xf ctrl=I2C0 addr=0x2c speed=400000 gpio=GPP_?,pin=N,lvl-low desc=0x20`

### 3b. `drivers/src/dw_i2c.rs` — DesignWare master, POLLED
Probe `pci::find_all_devices(0x8086, 0x9d60|0x9d61)` (also 9d62/9d63 harmless), `map_bar(0, CacheMode::Uncached)`
(`drivers/src/pci.rs:886`), enable MEM+BUSMASTER in PCI cmd, PMCSR→D0 (PM cap, write bits1:0=0, wait 10 ms).
LPSS private block at BAR0+0x200: write `0x204 RESETS = 0x7` (func+idma out of reset, as Linux intel-lpss), read
`0x2FC CAPS` for the log. Check `IC_COMP_TYPE @0xFC == 0x44570140` else bail ("dw-i2c: not DesignWare").
Registers: CON 0x00, TAR 0x04, DATA_CMD 0x10 (bit8 READ, bit9 STOP, bit10 RESTART), SS_HCNT 0x14, SS_LCNT 0x18,
FS_HCNT 0x1C, FS_LCNT 0x20, INTR_MASK 0x30 (=0, polled), RAW_INTR_STAT 0x34, RX_TL 0x38, TX_TL 0x3C, CLR_INTR 0x40,
CLR_TX_ABRT 0x54, ENABLE 0x6C, STATUS 0x70 (bit0 ACTIVITY, bit2 TFE, bit3 RFNE, bit5 MST_ACTIVITY),
TXFLR 0x74, RXFLR 0x78, SDA_HOLD 0x7C, TX_ABRT_SOURCE 0x80, ENABLE_STATUS 0x9C, COMP_PARAM_1 0xF4 (FIFO depth
= ((v>>8)&0xFF)+1 tx, ((v>>16)&0xFF)+1 rx).
Init: ENABLE=0 (poll ENABLE_STATUS bit0 == 0, ≤ 25 ms), CON = MASTER(1)|SPEED_FAST(2<<1)|RESTART_EN(1<<5)|
SLAVE_DISABLE(1<<6) = 0x65; FS HCNT/LCNT/SDA_HOLD from FMCN, fallback (120 MHz SPT clock) HCNT 0x48, LCNT 0xA0,
HOLD 0x1E; SS from SSCN, fallback 0x1B0/0x1FB/0x1E; INTR_MASK=0; RX_TL=0; TX_TL=0.
API: `pub fn xfer(bus: usize, addr: u16, w: &[u8], r: &mut [u8]) -> Result<(), u32 /*abrt src or ETIMEDOUT*/>`:
ENABLE=0, TAR=addr (bit12 for 10-bit), ENABLE=1; push writes (last one STOP if r empty); then reads: one DATA_CMD
READ per byte (first gets RESTART if w nonempty, last gets STOP), interleaved with RX drain so TX FIFO never exceeds
depth; check RAW_INTR_STAT bit6 TX_ABRT → read TX_ABRT_SOURCE, CLR_TX_ABRT, Err; wait STOP_DET (raw bit9) then
CLR_INTR. Timeout 50 ms via `crate::i915::now_ns()`. Per-bus `spin::Mutex`. Max transfer 4 KiB (report descriptors
are < 1 KiB). Self-test line: `dw-i2c: 00:15.0 up comp=0x44570140 fifo=64/64 fs=0x48/0xa0/0x1e (FMCN)`.

### 3c. `drivers/src/hid_desc.rs` — pure-logic HID report-descriptor parser (alloc only, host-testable)
Items: page/usage/usage min-max/logical+physical min-max/unit exp/report size/count/id, collections (track a stack;
each Field gets `app: u32` and `link: u16` = index of the innermost Logical collection), main items Input(0x80)/
Output(0x90)/Feature(0xB0) → `kind`. Field: `{kind, report_id, bit, size, count, flags, lmin, lmax, pmin, pmax,
unit, unit_exp, usages: Vec<u32>, app, link}`. Plus `fn summary(&Descriptor) -> String` for the log.
PTP extraction `pub struct Ptp { input_id: u8, fingers: Vec<Finger{tip, conf?, cid, x, y}>, contact_count,
button1, scan_time?, x:(lmin,lmax,pmin,pmax,unit_exp), y:(..), mode_feature: Option<(id, bit, size)> /* 0x000D0052 */,
max_contacts_feature: Option<..> /* 0x000D0055 */ }` from the app collection 0x000D0005 (Touch Pad).
Mouse fallback: app 0x00010002 → buttons + X/Y rel + wheel.
Host test: `drivers/hid-desc-tests` (copy the pattern of `drivers/bthid-tests`) with a captured Synaptics PTP
descriptor (use a public one, e.g. the Windows PTP sample from Microsoft docs) asserting 5 fingers, X/Y ranges.

### 3d. `drivers/src/i2c_hid.rs` — HID-over-I2C (spec 1.0) + evdev glue
For each live PNP0C50 slave with `hid_desc_reg`:
1. Read HID descriptor: `xfer(addr, [reg_lo, reg_hi], 30 bytes)`; check wHIDDescLength==30, bcdVersion==0x0100.
   Fields: repDescLen@4, repDescReg@6, inputReg@8, maxInput@10, outputReg@12, maxOut@14, cmdReg@16, dataReg@18,
   VID@20, PID@22, ver@24. Log: `i2c-hid: TPD0 desc 06 00 ... (30 B) vid=06cb pid=xxxx rd_len=N in=0x.. max_in=..`
   plus the raw 30 bytes hex.
2. SET_POWER ON: write `[cmd_lo, cmd_hi, 0x00, 0x08]`; sleep 1 ms. RESET: write `[cmd_lo, cmd_hi, 0x00, 0x01]`;
   then poll the input register (read 2 bytes) up to 100 ms until it returns `00 00` (reset ack).
3. Report descriptor: `xfer([rd_lo, rd_hi], repDescLen)`; log length + crc32 + `hid_desc::summary`.
4. If PTP: SET_REPORT feature Input Mode = 3 (touchpad):
   `[cmd_lo, cmd_hi, 0x30 | id (id<15; else 0x3F and append id byte), 0x03, data_lo, data_hi, 0x04, 0x00, id, 0x03]`
   (length = 2 + 1 + 1 payload). Also GET_REPORT max contacts (opcode 0x02) for the log only.
5. Poll thread `ki2chid` (spawn like `kernel/src/lappower.rs:47`): every 8 ms read `max_in` bytes from inputReg;
   len = u16[0..2]; len 0 or 2 ⇒ nothing; else report = buf[2..len]. (Phase 2: gate the read on the GpioInt pad
   RX state — the INT344B GPIO controller's `_CRS` gives its community MMIO; PADCFG DW0 bit1 = RX state, level-low
   asserted. Skip in phase 1.)
6. Map PTP input → evdev MT (type-B slots): per finger with tip && conf: `ABS_MT_SLOT i, ABS_MT_TRACKING_ID cid`
   (new contact ⇒ new tracking id counter; lifted ⇒ -1), `ABS_MT_POSITION_X/Y`; then `BTN_TOUCH`, `BTN_TOOL_FINGER/
   DOUBLETAP/TRIPLETAP/QUADTAP` from contact count, `ABS_X/ABS_Y` = first contact, `BTN_LEFT` from button1,
   `MSC_TIMESTAMP` from scan time (100 µs units ×100 → µs), `SYN_REPORT`. Hybrid-mode reports carry fewer fingers
   than max: honour contact_count on the first report of a frame and accumulate (Linux hid-multitouch semantics).
7. Non-PTP (mouse-mode) or other HID collections: log only in this lane.

### 3e. evdev: a kernel-created MT touchpad node
Extend uinput rather than adding a new hardcoded kind:
- `servers/evdev/src/uinput.rs`: add `props: u8` to `Ctx` (+ `CTX0`), handle `UI_SET_PROPBIT` (`_IOW('U', 110, int)`)
  so bthid benefits too; add
  `pub fn create_kernel(name: &str, bus: u16, vid: u16, pid: u16, evbits: u32, keys: &[u16], abs: &[(u16, [i32;6])], props: u8, slots: u8) -> Option<u32>`
  that claims a free Ctx with a synthetic `open_id = 0xFFFF_0000 | idx`, fills it, sets `created`, and calls
  `register_input_device(DEVKIND_UINPUT_PAD, …)` (kind chosen by BTN_TOOL_FINGER 0x145 in keys — extend the
  `create()` test at `uinput.rs:98` to also treat 0x145 as PAD). Also `pub fn props(dev_id) -> u8`, `pub fn slots(dev_id) -> u8`.
- `servers/evdev/src/lib.rs:1495` EVIOCGPROP: `if uinput::is_kind(kind) { buf[0] = uinput::props(dev_id) }`.
  Touchpad props: INPUT_PROP_POINTER(0) | INPUT_PROP_BUTTONPAD(2) (clickpad: single button1 usage).
- `lib.rs:~1586` add `0x0a => EVIOCGMTSLOTS`: read the request `code` (first u32 of the user buffer), answer
  `[code, v0..v(n-1)]` where n = min((size/4)-1, slots); values = current per-slot value (keep a small per-device
  slot table updated in `push_event` when type=EV_ABS code in 0x2f..0x3d) or 0 / -1 for TRACKING_ID. Without this
  libevdev_set_fd fails and libinput ignores the device.
- Absinfo for the pad: X/Y and MT_X/MT_Y: min/max = logical, resolution = units/mm from physical extents
  (`(lmax-lmin) / ((pmax-pmin) * 10^unit_exp_in_mm)`; HID unit cm, exp -2 typical ⇒ res ≈ 30-40), MT_SLOT 0..max-1,
  TRACKING_ID 0..65535, evbits EV_SYN|EV_KEY|EV_ABS|EV_MSC, MSC bit MSC_TIMESTAMP (needs the msc bitmap in Ctx; if
  absent skip MSC_TIMESTAMP entirely — libinput does not require it).
Name: `"<HID> Touchpad"` e.g. `"SYNA3071:00 06CB:xxxx Touchpad"` (libinput quirks match on this), bus BUS_I2C 0x18.

## 4. Knobs (kernel cmdline, `crate::nvidia_probe::cmdline_word`)
- `i2c=0` — no DesignWare init at all (default 1 on x86_64; inert if no 9d60/9d61).
- `i2c_hid=probe|1|0` — default `probe`: bus up + slave scan + descriptors + report-descriptor parse logged, NO
  input node, NO Input Mode switch (zero behaviour change). `1`: create MT node, switch PTP mode, poll thread.
- `touchpad=ps2|i2c` — default `ps2`. With `i2c` and a live PTP node, psmouse drops the touchpad's PS/2 packets
  (keep trackstick/buttons): gate in `psmouse::byte()` (`drivers/src/psmouse.rs:169`) on a new
  `pub static PAD_I2C: AtomicBool` set by i2c_hid after the first valid PTP report. Fallback = reboot without knob.
- `i2c.debug=1` — hex-dump every transfer.

## 5. Wiring (minimal shared-file touches)
- `drivers/src/lib.rs:23` add `#[cfg(target_arch="x86_64")] pub mod i2c_acpi; pub mod dw_i2c; pub mod i2c_hid;`
  and `pub mod hid_desc;` (all arches; pure logic).
- `kernel/src/lappower.rs:45` after `acpi_power::init(...)`: `drivers::i2c_hid::init(cmdline)` (does dw_i2c probe +
  i2c_acpi::scan + descriptors; spawns `ki2chid` only when mode=1). Must run after evdev is up — for node creation
  retry from the poll thread like `power_sysfs::register_input` (`drivers/src/power_sysfs.rs:202`).
- QEMU has no 9d60 ⇒ everything inert; log `i2c: no LPSS I2C controllers`.

## 6. If TPD0 is NOT live (PS/2-only pad)
Report it with the `_STA`/`_HID` values. Then read the TPS6598x at UCM1/UCM2 `_CRS` addresses (usually 0x38/0x3F):
TPS registers are "4CC": write `[reg]`, read `[len, data…]`: 0x00 VID (u32, "0x00000451"→TI), 0x01 DID,
0x03 MODE (4 ASCII: "APP " expected), 0x1A STATUS (u32: bit0 plug present, bits 2-3 conn state, bit5 port role,
bit6 data role, bits 20-21 VBUS status), 0x3F POWER_STATUS, 0x40 PD_STATUS. Log decoded line per controller:
`tps6598x: UCM1 @0x38 mode=APP  status=0x... plug=1 role=sink data=ufp vbus=vSafe5V`. Read-only, no 4CC commands.

## 7. Test plan
1. Host: `cargo test` in `drivers/hid-desc-tests` (descriptor parse: 5 fingers, X 0..lmax, Input Mode feature found).
   Show one assertion failing first (e.g. finger count before link-collection tracking) per testing.md.
2. QEMU (x86_64 is the laptop arch; aarch64 build only must compile since modules are cfg'd):
   `./scripts/build-all.sh --arch x86_64` (INTEL=1 for the laptop image), boot via
   `$SP/lock.sh qemu python3 .claude/skills/run-leandros/runtests.py x86_64 lapi2c /bin/evdevtest` (or default regress)
   with `LEANDROS_RUN_ID=lapi2c-x86`; expect `i2c: no LPSS I2C controllers` and green. Also `uinputtest` (uinput
   props/MTSLOTS changes) on aarch64: `runtests.py aarch64 lapi2c-a64 /bin/uinputtest`.
   Add an `uinputtest` case: create a pad with ABS_MT_SLOT via UI_SET_PROPBIT/ABS setup, then `EVIOCGMTSLOTS`
   returns `[code, …]` and `EVIOCGPROP` has bits 0|2.
3. Laptop (hold `$SP/lock.sh laptop` only during deploy→measure; boot `good` back after): build from current main,
   `laptop-deploy-kernel` trial with cmdline = current good extra string + ` i2c_hid=probe` (NEVER bare). Collect:
   `remote.py run 'dmesg | grep -E "dw-i2c|i2c-acpi|i2c-hid|tps6598x"'`. Expected:
   - `dw-i2c: 00:15.0 up comp=0x44570140 …` and `00:15.1 up …`
   - `i2c-acpi: …TPD0 hid=… sta=0x…` (+ TPL0/TPL1/CPD0/UCM1/UCM2/NFC1/IMP3 lines with sta)
   - `i2c-hid: TPD0 desc 1e 00 00 01 … vid=06cb pid=…` (30 hex bytes) and `report desc N B crc=… apps=[0d:05 touchpad,
     01:02 mouse, …] ptp fingers=5 x=0..NNNN (pmin..pmax, res R/mm) mode_feature id=…`
   Then second trial with ` i2c_hid=1`: `ls /dev/input`, idle read `timeout 3 evtest-like dump` (no touches possible)
   shows zero events and no errors; `cat /proc/bus/input/devices`-equivalent lists the pad with props 0x5. libinput
   check if present: `libinput list-devices` → Capabilities: pointer gesture, "Tap-to-click", "Scroll methods: *two-finger".
4. Merge batch: both arches regress once on main (box farm `$SP/box/regress.sh lane/lapi2c x86_64`).

## 8. Risks
- AML `_DSM`/`_STA` of TPD0 may touch GNVS/EC fields: run scan inside `with_ns` in task context only (same lock as kacpid).
- If `_PS0` of the I2C controller/pad is missing the pad may stay powered off: also evaluate the device `_PS0` and
  any `_PR0` power resources' `_ON` before the descriptor read; log failures, never loop.
- 10-bit or wrong timings ⇒ TX_ABRT 7B_ADDR_NOACK (abrt src bit0) on every slave: compare UCM1 (always powered)
  vs TPD0 to separate bus bring-up bugs from a powered-off pad.
- Do not leave PTP mode set when `touchpad=ps2`: only send Input Mode in `i2c_hid=1`.
