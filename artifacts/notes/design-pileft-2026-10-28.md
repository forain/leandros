# design pileft (2026-10-28): the seven remaining Pi 5 items

Base: main 7f0b2766+. Pi good = 8e2fd94ff6e7 (PCIe on by default, RP1 eth0 + xHCI usb0/usb1, Wi-Fi, BT pad).
Lanes test aarch64 only (QEMU). Only the orchestrator touches the Pi, under `$SP/lock.sh pi`.
Recover with the serial reset (`printf '\035\035\035\035\035' > /dev/cu.usbserial-1410`) or `pi-boot good`.
Design only; nothing below was run on hardware. Line numbers are from 7f0b2766.

---------------------------------------------------------------------------------------------------
## 0. Step zero: one observation session on the CURRENT good kernel (no code, orchestrator + user)

Three items depend on facts only the hardware can give. Collect them in a single Pi session first:
1. `v3d.probe=1` trial boot of the current good kernel. The pv3dpwr2 power sequence (GRAFX POWUP ->
   V3DRSTN -> clk -> confirm, SError survival) is merged but has never run on the Pi: the scratchpad
   logs show 108x `[v3d] skipped (default off; v3d.probe=1 enables)` and the only real-Pi probe result
   is the pre-pwr2 `[v3d] fail sms-power timeout val=0x00000050 pm=0x00001040->0x00001040`.
   Pass the knob through pi-deploy's kexec args path (the "args" support merged in kexec phase 2;
   check `remote.py pi-deploy --help` for the flag name). Save every `[v3d]` line.
2. USER: plug the 8BitDo Lite 2 USB-C cable into a Pi USB port, once per mode position of its mode
   switch. Save the enumeration lines (`[USB]`/`[USB-HID]`: VID:PID, class/subclass/protocol,
   `role_name`). Today a subclass-0 HID interface logs "HID (report protocol only, unsupported)"
   (drivers/usb/src/hid/mod.rs:403); a vendor-class XInput interface (ff/5d/01) gets no driver.
   This decides which of lane A's two paths is needed first (expected: X mode = 045e:028e ff/5d/01,
   D mode = 2dc8:xxxx HID subclass 0, S mode = 057e:2009 HID subclass 0 that needs a USB handshake).
   Also plug a USB keyboard once: it proves the existing boot-protocol HID binds on RP1 xHCI
   (`usb_hid::prepare/init` run around `rp1_usb::probe_pending`, drivers/src/usb_hcd.rs:140-147).
3. `sndtest info` (twice, 5 s apart, during `sndtest tone 440 10`) for the HDMI audio counters, and
   USER: listen for the tone (the one listening step of item 7).
4. `cat /proc/thermal` and the boot line `[rp1fan] tach N rpm` (is a fan attached at all?).

---------------------------------------------------------------------------------------------------
## 1. USB gamepad over cable (8BitDo Lite 2 into RP1 xHCI)

### Findings
- The USB HID class driver is the shared `usb` crate (drivers/usb/src/hid/mod.rs), so it is the
  same code on x86 and on the Pi's RP1 xHCI, and it IS wired on the Pi path (usb_hcd.rs:140-147).
  But its scope is boot-protocol keyboards and mice only (hid/mod.rs:1-16, accept test at :100-103).
  A gamepad never binds on ANY arch: it is subclass 0 (report protocol) or vendor class (XInput).
- The proven gamepad logic is userland: bthid's `report.rs` (pure, host-tested by
  drivers/bthid-tests) parses report descriptors, normalises pads Linux-style, has quirks
  (`quirk_for`, report.rs:290, 057e:2009 SwitchPro), and emits through /dev/uinput (bthid main.rs:134).
- There is no hidraw in the tree.

### Design: Linux-shaped hidraw + bthid as the one HID host for both transports
Chosen over an in-kernel gamepad mapper because (a) usb hid `poll` runs in IRQ context on the BSP
tick, where report.rs's allocating mapper must not run, (b) it reuses the 8BitDo/Switch quirks already
proven over BT instead of duplicating them in the kernel, (c) /dev/hidrawN is what SDL/hidapi expect.

Kernel (usb crate + glue):
- drivers/usb/src/hid/mod.rs `parse_config` (:71) gains two roles beside keyboard/mouse:
  - `ROLE_RAW`: class 3, subclass 0 (any protocol), or a boot interface beyond the first.
    At enumeration (task context): read the HID descriptor (0x21) length, GET_DESCRIPTOR(0x22)
    the report descriptor (cap 4 KiB), SET_IDLE(0), keep report protocol (no SET_PROTOCOL).
  - `ROLE_XINPUT`: class 0xff, subclass 0x5d, protocol 0x01 (Xbox 360 wired). No descriptor.
    Interrupt IN (and OUT if present) endpoints opened like a HID pipe.
  - `role_name` (:398): "HID raw (hidraw)" / "XInput (hidraw)"; the "unsupported" text stays for
    `usb.hidraw=0`.
- `poll` (:284): raw pipes copy each completed IN report (<=64 B) into a fixed per-device ring (no
  allocation in IRQ context; 64 slots x 64 B, drop-oldest with a counter) and mark the node readable.
- New drivers/src/usb_hidraw.rs: `/dev/hidrawN` char device, registered the way /dev/uinput is
  (worker: `git grep -n uinput servers/evdev kernel/src` for the device-node table), N allocated on
  attach, removed on detach (`detach`, hid/mod.rs:355). ABI (Linux values):
  - read(): one report per call; EAGAIN with O_NONBLOCK; poll POLLIN.
  - write(): to the interrupt OUT endpoint, else SET_REPORT(Output) on ep0 (needed for the Switch
    Pro USB handshake and XInput LEDs).
  - ioctl HIDIOCGRDESCSIZE 0x80044801, HIDIOCGRDESC 0x90044802, HIDIOCGRAWINFO 0x80084803
    (bustype 3, vendor, product), HIDIOCGRAWNAME(len) 0x80ff4804 style (len in size field).
  - XInput nodes report an empty descriptor (size 0); that is a LeandrOS extension, bthid keys on
    VID:PID or the empty descriptor.
- Knob `usb.hidraw=0`: old behaviour. Inert on failure: descriptor fetch / endpoint setup failure
  logs `[USB-HID] <name> raw setup failed: <stage>` and leaves the interface unconfigured, exactly as
  "unsupported" today. Keyboards/mice keep the boot path unchanged.

Userland (bthid):
- userland/bthid/src/report.rs: `Quirk::XInput` for 045e:028e (and 0x2dc8 PIDs seen in step zero in X
  mode): fixed 20-byte report `00 14 | btn lo | btn hi | LT | RT | LX LY RX RY (i16 LE)` mapped to
  the same normalised pad (Y axes inverted like xpad). `Quirk::SwitchPro` gains a USB init sequence
  (`80 02` handshake, `80 04` no-timeout; written with hidraw write) beside the BT one (:308).
- userland/bthid/src/main.rs: a USB source next to the BT one: scan /dev/hidraw* every 1 s (no
  hot-plug events exist), HIDIOCGRAWINFO + HIDIOCGRDESC -> `HidDevice::new(vendor, product, desc)`
  (or the XInput quirk), read reports -> existing uinput emitter. One uinput pad per hidraw node,
  destroyed on read ENODEV. bthid must run without btd: when btd's socket is absent it keeps serving
  USB and retries btd with backoff (today "btd went away; restarting" - make it non-fatal).
- init must start bthid even when there is no /dev/hci0: that one-line change is in lane B
  (init owner): `bt_services()` (init main.rs:899) starts bthid unconditionally unless `bthid=0`,
  btd still only with /dev/hci0.

### Tests
- Host: drivers/bthid-tests: XInput report vectors (all buttons, stick extremes, triggers), Switch
  USB init bytes; usb crate host tests for `parse_config` with synthetic ff/5d/01 and 3/0/0 config
  descriptors (mutation guard: drop the subclass-0 accept -> red).
- QEMU aarch64: full regress unchanged (usb-kbd/usb-tablet boot path). If QEMU's `usb-tablet`
  enumerates as subclass 0 (check today's "unsupported" line in a QEMU boot with `-device usb-tablet`),
  use it as the hidraw vehicle: /dev/hidraw0 appears, HIDIOCGRDESC returns its 74-byte descriptor,
  QMP `input-send-event` motion produces reads. bthid in QEMU: `bthid: usb <vid:pid> roles=mouse`.
- Pi (orchestrator + USER plugs the cable; button presses optional):
  `[USB-HID] 8BitDo ... (slot N interface 0) XInput (hidraw)` or `HID raw (hidraw)`,
  `bthid: usb 045e:028e hidraw0 roles=gamepad`, a new /dev/input/eventN, `evtest` shows the
  ABS/BTN caps without presses; with USER presses: BTN_SOUTH 1/0, ABS_X swing. Unplug ->
  `[USB-HID] ... removed`, bthid `usb hidraw0 gone`, event node gone. BT pad keeps working.

---------------------------------------------------------------------------------------------------
## 2. Fan curve (RP1 PWM1 ch3, GPIO45)

### Findings
- drivers/src/rp1_fan.rs already programs the PWM and runs a FIXED state 3 (175/255) because "no
  thermal yet" (rp1_fan.rs:6-8). The thermal source now exists: `thermal::soc_millicelsius()`
  -> `rpi_misc::soc_millicelsius` (AVS monitor, firmware GET_TEMPERATURE fallback, rpi_misc.rs:135).
- The curve exists and is unused: `rp1_hw::fan_state(temp_mc, cur)` (rp1_hw.rs:275), trips
  50/60/67.5/75 C (:273), hysteresis 5 C (:274) = Linux DT cooling map.
- A periodic kthread already exists: pi5_board.rs:94 loop, POLL_MS=50 (:23); it reads the tach once at
  5 s (:108-117).

### Design
- rp1_fan.rs: store the clock rate at init (`CLK` atomic) so `set_state(s)` needs no clk argument;
  add `pub fn tick(temp: Option<i32>)`:
  - `fan=auto` (default): `s = fan_state(t, cur)`; t >= 85 C forces state 4 regardless.
  - temp None 3 ticks running -> hold FIXED_STATE, log once `[rp1fan] no temperature; holding state 3`.
  - On a change log `[rp1fan] 62.4 C state 1->2 level 125/255` (only transitions, so it is bounded).
- pi5_board.rs loop: every 20th iteration (1 s) once RP1_UP: `rp1_fan::tick(thermal::soc_millicelsius())`.
- Knobs: `fan=auto|off|0..4` (`off` = state 0, `N` = fixed state N, both still logged at init).
- thermal.rs `arch_thermal_report`: add a line `fan: state=S level=L/255 rpm=R mode=auto` (rpm from
  `rp1_fan::rpm()`), so tests read it from /proc/thermal with no new interface.
- Inert: PWM not initialised (BASE==0) -> tick is a no-op; no RP1 (pcie.rpi5=0) -> nothing runs.

### Tests
- Host: rp1_hw fan_state table test incl. hysteresis edges (rise at 60.0, hold at 55.1, drop at 54.9)
  and the 85 C override. Mutation guard: remove hysteresis -> red.
- QEMU aarch64: regress (no RP1, code path inert; /proc/thermal unchanged "no sensor" + no fan line).
- Pi (no user step unless no fan is attached): boot -> `[rp1fan] ... (auto)`; idle `cat /proc/thermal`
  -> `soc: 4xxxx`, `fan: state=0|1`; load 4x `yes > /dev/null` for 3 min -> state rises, rpm rises;
  kill load -> drops after hysteresis. `fan=3` -> fixed, as before.

---------------------------------------------------------------------------------------------------
## 3. Real Bluetooth bdaddr

### Findings
- Plumbing exists: bringup.rs:405-411 writes `Options.bdaddr` with BCM WRITE_BD_ADDR when the
  controller reports the placeholder; boot.rs:362 passes `dt_addr` from `fdt_local_bd_address`
  (bringup.rs:420-460), which returns None for all-zero. On the Pi the DT value is zero (the stock
  .dtb / kexec DTB carries zeros), so the placeholder 43:45:C0:00:1F:AC stays (same on every Pi).
- bcm2712_otp.rs already reads the factory Ethernet MAC via the mailbox (GET_BOARD_MAC_ADDRESS
  0x00010003) and derives Wi-Fi = Eth+1 (`wifi_from_eth`, :74-75). Pi convention: BT = Eth+2.

### Design
- bcm2712_otp.rs: `pub fn bt_addr() -> Option<[u8;6]>` = eth MAC + 2 (same carry rule as
  `wifi_from_eth`), `mac_usable` checked.
- bt/boot.rs before :362: `addr = knob or dt_addr or otp` where knob `bt.bdaddr=`:
  `auto` (default: DT if non-zero, else OTP), `dt`, `otp`, `keep` (placeholder, today's behaviour),
  or an explicit `xx:xx:xx:xx:xx:xx`.
  BYTE ORDER: DT `local-bd-address` and HCI are little-endian (LSB first). The worker must convert
  the MAC (MSB first) into whatever `BdAddr` holds, proved by a host test that formats the OTP-derived
  BdAddr as `D8:3A:DD:..` (MSB first) and that the WRITE_BD_ADDR payload is LSB first.
- boot.rs:385 source label: `set-from-dt` | `set-from-otp` | `set-from-cmdline` | `placeholder`.
- Check ordering: `bcm2712_otp::boot()` must have run before BT bring-up (it feeds brcmf already;
  if BT can run first, call it lazily from `bt_addr`).
- Inert: no OTP MAC or write fails -> keep placeholder, log `bdaddr=... (placeholder: otp unavailable)`;
  a WRITE_BD_ADDR error is non-fatal (bring-up continues).
- Consequence: the 8BitDo is paired to 43:45:C0:00:1F:AC; after the change it must be RE-PAIRED once
  (USER: hold the pad's pair button; btd/bthid pairing flow as in pibtreconn). `bt.bdaddr=keep`
  restores the old address if the re-pair cannot be done in that session.

### Tests
- Host: bt crate test of eth->bt derivation (incl. carry ff->00), byte order, knob parsing.
- QEMU aarch64: regress (no BT UART; no change). x86 btusb/btintel unaffected (OTP path rpi5-only).
- Pi: `[bt] hci ver=... bdaddr=D8:3A:DD:xx:xx:yy (set-from-otp)`, btd `ready bdaddr=<same>`; it must
  equal the Ethernet MAC + 2 (`ip link`/`/proc/net/nic` eth0 MAC). USER: re-pair the pad, then a kexec
  trial reconnects with no commands.

---------------------------------------------------------------------------------------------------
## 4. Init: skip greetd when there is no GPU

### Findings
- greetd is spawned when `graphical_login_wanted()` (init main.rs:1438) says so (main.rs:562);
  on the Pi gpu-env exits 78 (DM_EXIT_NO_GPU, :500), handled at :673-705 (NO GPU banner, then
  `sweep_strays`). pibtauto fixed the btd/bthid kill; the remaining cost is a wasted greetd/COSMIC
  start, a console repaint and a sweep on every Pi boot.
- A plain `/dev/dri` existence test is NOT safe: the Pi may expose a stub DRM card (v3d stub /
  scanout) and NVIDIA appears late (lane nvwait). gpu-env is the single source of truth.

### Design
- gpu-env (find with `git ls-files | rg gpu-env`) gains `--check`: run its renderer detection
  (including its NVIDIA wait) and exit 0 (hardware GL) or 78, starting nothing.
- graphical_login_wanted(): after the existing checks, if `/bin/gpu-env` exists and `dm.precheck`
  is not `0`, run `/bin/gpu-env --check` with a 120 s bound (fork/exec/wait4 with WNOHANG polling).
  Exit 78 -> print the existing NO GPU banner (factor it out of :697-703) and return false; any other
  status, a timeout or a missing gpu-env -> return true (old path, inert on failure).
  `/etc/leandros/allow-software-render` keeps today's behaviour (gpu-env already honours it).
- Lane-A support line (same file, same lane): `bt_services()` (:899) starts bthid unconditionally
  unless `bthid=0`; btd keeps its /dev/hci0 condition.

### Tests
- QEMU aarch64 with GL (default `--venus`/virgl run): greetd starts as before (desktop regress
  green). QEMU aarch64 with no GL device (the regress no-GPU variant): `graphical login off: no GPU
  renderer (gpu-env --check)`, NO `killed N stray process(es)`, NO greetd in `ps`.
- Regress grep: fail on `NO GPU RENDERER` appearing AFTER a greetd spawn in the no-GPU variant.
- Pi (initrd only, pi-deploy trial, no pi-sync): boot log shows the banner once, no greetd lines, no
  sweep; `ps` shows bthid (and btd); getty works.
- x86 laptop/PC risk is covered by the per-batch both-arch run: i915 and NVIDIA must still start COSMIC.

---------------------------------------------------------------------------------------------------
## 5. Pi root lacks grep (and dmesg, rg)

### Root cause (from the tree, not the Pi)
- There is no `grep` in ANY LeandrOS image, by design: build-all.sh:944 "the guest image ships no
  grep"; ripgrep `rg` is shipped instead (f00efe33, 2026-10-25), from the sibling checkout
  `../ripgrep` (mkfs-f2fs-populated.py:1883-1887).
- Every optional binary in mkfs-f2fs-populated.py is added only `if os.path.exists(p)` and silently
  skipped otherwise (rg, coreutils :532-536, disktester, ...). The Pi card was prepared before rg was
  built for aarch64 / before /bin/dmesg existed (userland/dmesg, 04819f7d, 2026-10-27), and pi-sync
  (45 pending changes) has not run since. Worktrees are siblings of the main checkout
  (/Users/forain/code/leandros-*), so `../ripgrep` and `../coreutils` resolve from them too; the
  aarch64 rg exists now (../ripgrep/target/aarch64-unknown-linux-musl/release/rg).
- The Pi login PATH is /bin only (init exports /bin:/usr/bin at main.rs:805/1080/1111/1140; the shell
  profile on the card apparently does not), so /sbin and /usr/sbin tools look absent too.

### Fix
- Ship a `grep`: new userland/grep (tiny Rust, musl, built like spawnwedge with its own build.sh) that
  translates POSIX grep arguments and execs /bin/rg: `--no-config --no-ignore --hidden -N`
  (no line numbers unless -n), `-` added when no file operand (grep reads stdin; rg would search
  cwd), `-r/-R` -> recursion, no operand dir without -r -> "is a directory" like grep,
  `-G` (default BRE) -> translate `\( \) \| \+ \? \{ \}` to ERE, `-E` passthrough, `-F,-i,-v,-c,-l,-L,
  -n,-h,-H,-q,-s,-w,-x,-o,-e,-f,-m,-A,-B,-C` passthrough; exit codes 0/1/2 kept. Host-test the
  translation table. Packed as /bin/grep and /bin/egrep, /bin/fgrep (argv0).
- mkfs-f2fs-populated.py: collect skipped optional binaries and print `MISSING optional: rg
  (../ripgrep/...)` lines plus a one-line summary at the end; `LEANDROS_REQUIRE=rg,grep,dmesg,
  coreutils` makes them fatal.
- pi_sync.py `build_manifest` (:73): set `LEANDROS_REQUIRE=rg,grep,dmesg,coreutils` by default
  (`--allow-missing` to override) so a pi-sync can never again silently drop them; `--dry-run --list`
  shows them as `new`.
- /etc/profile (populate): `PATH=/bin:/sbin:/usr/bin:/usr/sbin` for login shells.
- build-all.sh: build userland/grep for both arches next to build_ripgrep (:1116).

### Tests
- Host: grep shim translation tests; scripts/pi_sync_test.py: manifest without /bin/rg fails with the
  MISSING message, `--allow-missing` passes.
- QEMU aarch64: `echo foo | grep -c foo` -> 1, `grep -n root /etc/passwd`, `grep -r PATH /etc`,
  `grep 'a\|b'` BRE alternation, `grep -q x /nonexistent; echo $?` -> 2.
- Pi (orchestrator, ~25 min, also delivers the 45 pending changes): `remote.py pi-sync --dry-run
  --list` lists /bin/grep, /bin/rg, /bin/dmesg; after pi-sync: `grep root /etc/passwd`, `dmesg | grep
  v3d`, `echo $PATH` in a fresh SSH login.

---------------------------------------------------------------------------------------------------
## 6. V3D GPU: next hardware step and screen-free verification

### Next concrete step
Step zero item 1 (`v3d.probe=1`, current good kernel, no code). The ladder and its meaning:
- `[v3d] ... grafx=<before>-><after>` power lines, then `[v3d] ident ver=7.1...` -> pwr2 fixed power.
- `[v3d] mmu ... mode=...` (phase B), `[v3d] cl queue ...` (phase C).
- `[v3d] selftest clear ok crc=0xe8f5217c` (RCL clear readback, pv3d-cl) and `[v3d] selftest tfu copy
  ok perm=...` (pv3d-tfu-csd): these are already screen-free hardware render proofs.
Forks:
- `fail sms-power timeout` again with `grafx=` unchanged -> lane D adds a firmware power-domain step
  before the PM sequence: mailbox RPI_FIRMWARE_SET_DOMAIN_STATE (0x00038030) domain V3D (10) on,
  knob `v3d.pwr=pm|fw|both` (default both after proof), every step logged with PM_GRAFX + SMS
  readback. Files: drivers/src/v3d/power.rs, regs.rs.
- `fail serror stage=...` -> logged and inert by pwr2 (DEVICE cleared); stage tells which access
  faulted (power vs MMU/CL) -> design follow-up, do not iterate blindly.
- `selftest clear fail timeout|mismatch` -> phase C bug, the note lists the dumped registers.

### Screen-free verification after the ladder is green
- `scripts/pi-gpu-smoke.sh pi:<ip>` (ident / armed / vulkan fill / es31test compute / kmscube):
  expect `pi-gpu-smoke: 5/5 PASS`. kmscube's PASS is its frame count, not a picture.
- Add one deterministic GLES readback test (lane D): `es31test --fbo` (or a new `glreadback`):
  render to a 256x256 RGBA8 FBO: clear blue, one flat-shaded axis-aligned red quad covering pixel
  centres [64,192), no blending/MSAA; `glReadPixels`; compare against the analytic image (exact
  per-pixel), print `PASS fbo crc=0x........ bad=0/65536 renderer=V3D 7.1.x`. The analytic compare
  needs no reference GPU; the CRC is printed so the Pi result can be pinned as a constant once seen.
  QEMU aarch64 (virgl/Zink) must give the identical CRC: that is the cross-check.
- No user step needed for item 6. Screen check is optional.

---------------------------------------------------------------------------------------------------
## 7. HDMI audio: hardware proof without a listener

The code (lane pihdmi-audio, drivers/src/hdmi_audio/) runs a cyclic dma40 transfer paced by DREQ 10
from the MAI FIFO. Pacing is the proof: the DMA can only advance at the audio sample rate if the MAI
consumes samples, which needs the audio clock (N/CTS) and the MAI path to work.

### Check (no listening)
- Boot line: `[hdmiaud] ready: hdmi0 pix=148500 kHz N=6144 CTS=148500 gate=... dma ch10 fifo=cpu|bus irq|poll edid=hdmi`.
- During `sndtest tone 440 10`, `sndtest info` twice 5.0 s apart:
  - DMA position delta / elapsed = 384000 B/s (+-1 %) for 48 kHz x 2 ch x 4 B IEC958 subframes
    (or the configured rate x channels x 4). Much faster -> DREQ not pacing (FIFO address wrong);
    zero -> `no progress` path.
  - period/IRQ counter advancing at rate/period_frames (e.g. 46.9/s for 1024-frame periods).
  - MAI_CTL underflow/overflow sticky bits 0 after the first second; restart counter 0.
  - EDID CEA audio data block present with LPCM (sink accepts audio at all).
- If `sndtest info` lacks any of these, lane D adds them (fields: dma_pos_bytes, periods, irqs,
  underflows, mai_ctl, fifo, n, cts, gate) in hdmi_audio's info hook; no behaviour change.
- Pass criterion without a listener: rate within 1 %, sticky error bits clear, no `[hdmiaud] inert`.

### The single user step
USER (once, in the step-zero session): monitor speakers on, volume up, run `sndtest tone 440 5`:
"did you hear a 5 s beep?" A pass on the counters but silence -> InfoFrame/channel-status bits or the
sink's audio mute; then compare the audio InfoFrame RAM packet with Linux's vc4_hdmi values.

---------------------------------------------------------------------------------------------------
## Lanes (fast-worker), disjoint file ownership, order

| lane | items | owns | Pi user step |
|---|---|---|---|
| A `piusbpad` | 1 | drivers/usb/src/hid/**, drivers/usb/src/xhci (HID pipe setup only), drivers/src/usb_hid.rs, NEW drivers/src/usb_hidraw.rs, the hidraw node registration site, userland/bthid/**, drivers/bthid-tests/** | plug cable (+ optional button presses) |
| B `piinit` | 4, 5 (+ bthid unconditional start for A) | userland/init/src/main.rs, gpu-env script, NEW userland/grep/**, scripts/mkfs-f2fs-populated.py, scripts/pi_sync.py, scripts/pi_sync_test.py, scripts/build-all.sh | none |
| C `piboard` | 2, 3 | drivers/src/rp1_fan.rs, rp1_hw.rs (fan section), pi5_board.rs, thermal.rs, bcm2712_otp.rs, drivers/src/bt/boot.rs, bt/bringup.rs | re-pair the 8BitDo once |
| D `pigpuaud` | 6, 7 follow-ups | drivers/src/v3d/**, drivers/src/hdmi_audio/**, scripts/pi-gpu-smoke.sh, es31test/glreadback, sndtest info fields | listen for the tone once |

Order:
1. Step zero (orchestrator, Pi lock, current good kernel, no code): v3d.probe=1 trial, USB pad +
   keyboard plug-in (USER), sndtest info x2 + tone (USER listens), /proc/thermal + tach.
2. B and C in parallel (small, no Pi needed to code). Pi: one trial kernel with B+C (initrd + kernel),
   then pi-sync (ships grep/rg/dmesg + the 45 pending changes), then USER re-pairs the pad.
3. A after B is merged (needs the unconditional bthid start; uses the step-zero class tuple to pick
   XInput vs HID-raw first). Pi trial with the cable (USER plugs it).
4. D only as needed from step-zero results (power fallback, readback test, counters); its Pi boots
   need no user except the one listening step if step zero's tone was not yet heard.
