# Design: lapdisp2. External outputs from "listed" to "lit" (2026-10-27)

Inputs: `artifacts/notes/lane-lapdisp-2026-10-27.md` and `artifacts/notes/review-lapdisp-2026-10-27.md` (R1..R11).
Target hardware: the laptop (KBL-R i5-8350U, SPT/KBP PCH). The VBT has DDI-B (GMBUS pin 5, AUX B) and DDI-C (pin 4, AUX C), both DP++. eDP runs on DDI-A, transcoder EDP, pipe A, DPLL0.
Rules: every lane tests aarch64 (regression boot) and x86_64 QEMU `i915.fake=2 i915.kms=1`, then runs on the real laptop through the `laptop-deploy-kernel` skill with knobs. Real-monitor steps need the user to plug in a monitor; each test plan marks those steps **[MONITOR]**.

Lane order: **A** → (**B** ∥ **C1** ∥ **C2**) → **C3** → **C4**. A real monitor is first needed in A (the hidden-status check) and in C2.

---

## Lane A: `lapdisp-safe` (review fixes and extout on by default). Small.

The goal: `i915.extout` defaults to 1 with no compositor-visible risk.

1. **Hide extras without a CRTC (R1).**
   - `gpu_kms.rs` (seam at about :199): add `crtc_mask: fn(idx) -> u32` to `KmsOutputs`. i915 returns 0 until lane C4.
   - In `drm_device_interface.rs:ext_get_connector` (about :6949), report `connection = 2` and `count_modes = 0` when `crtc_mask == 0`, unless `i915.extout_show=1`. That knob is for debugging and drmsmoke; the default is 0.
   - GETENCODER (about :6990) returns `possible_crtcs = crtc_mask(i)`.
2. **Partial I2C-over-AUX fix (R2).** `outputs.rs:339-344`: request `min(16, 128-off)`. Copy `min(got, 128-off)`. Treat `got == 0` as DEFER, retrying up to 7 times, then fail.
3. **VBT (R3).** `outputs.rs:98`: `1|7=>1, 2|8=>2, 3|9=>3`. `outputs.rs:102`: read `c[0x19]`.
4. **Probe retry (R4).**
   - Add `fail: u8` and `next_ns: u64` to `Conn`.
   - After a probe that failed while HPD was high, keep `hpd = u8::MAX` so the next call re-probes. Allow at most 5 re-probes, at least 500 ms apart.
   - On connected-without-valid-EDID, set modes to 1024x768, 800x600 and 640x480 at 60 Hz (DMT timings: 65000 kHz 1024/1048/1184/1344 768/771/777/806 NH NV; 40000 kHz 800/840/968/1056 600/601/605/628 PH PV; 25175 kHz 640/656/752/800 480/490/492/525 NH NV).
   - Verify the EDID checksum (the 128 bytes must sum to 0 mod 256). Skip interlaced DTDs (`d[17] & 0x80`).
5. **HPD enable (R5).** In `outputs::init`, read `PCH_PORT_HOTPLUG` (0xC4030) and log it. Write `v & !(0x3 | 0x3<<8 | 0x3<<16) | 1<<4 | 1<<12 | 1<<20`; the status bits are W1C and must not be cleared by accident.
6. **GMBUS (R7).**
   - Around each transfer, set and then restore `SOUTH_DSPCLK_GATE_D` (0xC2020) bit 31.
   - Replace `m.wait(GMBUS2, ..)` with a loop that returns on HW_RDY (bit 11) **or** SATOER (bit 10), with a 50 ms bound per word.
7. **Stable eDP type (R10).** `drm_device_interface.rs:6876`: add a `KmsBackend` field `internal_panel: bool`. i915 sets it from `TRANS_DDI_FUNC_CTL_EDP` being enabled (`kms.rs:653`). Connector 1's type is `14 if internal_panel else 11`, independent of extout.
8. **Mode filter (R6), shared with C2/C3.** Add `outputs::mode_ok(conn, t)`:
   - HDMI: `clock ≤ 300000`.
   - DP: `clock*3 ≤ max_link_khz*max_lanes`, where `max_link_khz` is 162000/270000/540000 from DPCD 0x001 and the lane count comes from 0x002 bits 4:0. Capture both in the probe by reading DPCD 0x000..0x00F.
   - Both: `clock ≤ cdclk_khz()`, decoded from CDCLK_CTL `0x46000` bits 27:26 together with LCPLL1 (KBL: 337500/450000/540000/675000; port Linux `skl_get_cdclk`).
   - Drop the failing modes before they are copied out.
9. **Flip the default.** `opt("i915.extout") != Some(0)`.

Knobs: `i915.extout=0` (off), `i915.extout_show=1`, `i915.extout_aux=0` (unchanged).

Expected klog:
```
outputs: PCH_PORT_HOTPLUG 0x........ -> 0x........ (HPD B/C/D enabled)
outputs: 2 ports from VBT: DDI-B (gmbus pin 5, AUX B, DP, HDMI) DDI-C (gmbus pin 4, AUX C, DP, HDMI); 4 extra connectors, SDEISR=0x0.......
outputs: registered DP-1=disconnected HDMI-A-1=disconnected DP-2=disconnected HDMI-A-2=disconnected (no CRTC: hidden from clients)
outputs: HDMI-A-1 hpd=true -> connected (3 modes, preferred 1920x1080; 1 over limit dropped)   [MONITOR]
```

Tests:
- (1) QEMU x86_64 fake and an aarch64 boot: drmsmoke has 0 new failures, and `drmsmoke --list` shows eDP-1 or HDMI-A-1 as before.
- (2) Laptop, no monitor: greeter and login come up, `ps` shows cosmic-comp, and `drmsmoke --list` shows `eDP-1 connected` and 4 disconnected entries.
- (3) **[MONITOR]** Laptop with HDMI plugged in at boot: the session still comes up normally (the R1 regression check), klog shows the `connected` line, and `drmsmoke --list` still reports it disconnected (hidden).
- (4) Same as (3) with the USB-C DP adapter. Record which DDI each physical jack is (lane-note open item 3).
- (5) Re-run (3) with `i915.extout_show=1`. drmsmoke shows connected with EDID modes. Do not log in with this knob.

---

## Lane B: `lapdisp-hpd` (hotplug interrupt and DRM change uevent). Medium.

Goal: plugging or unplugging a monitor makes cosmic-comp rescan, and GETCONNECTOR never probes.

**Kernel, i915.**
1. Add the HPD interrupt.
   - In `vblank.rs` init (where it unmasks pipe A), program `SDEIMR` (0xC4004): clear bits 21|22|23. Program `SDEIER` (0xC400C): set the same bits.
   - Check that `GEN8_DE_PCH` is routed by the `MASTER_DE_PCH` bit (`vblank.rs:60`). It already is.
2. ISR, `vblank.rs:475-482`. The `(MASTER_DE_PCH, SDEIIR)` entry is currently ack-only.
   - When `v & (7<<21) != 0`: read `PCH_PORT_HOTPLUG` and write the same value back to clear its W1C status bits, then OR the status into `outputs::HPD_PENDING: AtomicU32`. Count this as "ours" so `IRQ_IDLE_STREAK` (`vblank.rs:487`) does not trip.
   - **Take no locks in the ISR**, and in particular not CONNS (see R9).
   - Bits [1:0] per port: 01 = short pulse, 10 = long pulse. Bits B=[1:0], C=[9:8], D=[17:16].
3. Add a kthread `i915-hpd` (spawn the same way as `exec.rs:650`).
   - Loop: if `HPD_PENDING` is set, swap it to 0 and handle it. Otherwise `sched::kthread::sleep_ns(..)`. Use a wake API from `sched::kthread` if one exists. If not, poll at 4 Hz only while extout is enabled; the getty-wakes rule keeps this well under 4/s.
   - On a long pulse, or a live-bit change seen by a 2 s backstop poll of SDEISR (for missed IRQs): wait 100 ms for the debounce, then run `outputs::reprobe(port)`. This is the current `info()` body moved here, including the A4 retries.
   - If any connector's `(connected, modes)` changed: `generation += 1` and send a uevent.
   - On a short pulse (DP IRQ_HPD): log it only. Link-status handling belongs to C3.
4. `outputs::info()` becomes cache-only, the same as `cached`. Keep the "first call before any IRQ" probe in `init` (`outputs.rs:156`).

**uevent path.** This is not netlink. The shim already uses `/dev/input/hotplug` from `servers/evdev`.
5. In `servers/evdev/src/lib.rs` (records at about :2192): add a record kind for DRM. Either extend `HpRec` with `drm: bool`, or add a `pub fn hp_drm_change(card: u8)` that pushes a record formatted as `change card0 drm 0 0 0` (`hp_format` is at about :2238). `drivers` already depends on `evdev-server` (`drivers/Cargo.toml:22`). Call it from the kthread and never from the ISR. Do not seed coldplug "add" records for DRM.
6. libudev shim, `ports/input-stack/shims/libudev/libudev.c`:
   - `udev_monitor_enable_receiving` (:1165): open the hotplug node when `monitor_wants(m, "input") || monitor_wants(m, "drm")`. smithay's `UdevBackend` filters on "drm" only.
   - `udev_monitor_receive_device` (:1207): parse `change card%d drm`. Return the static card0 `dev_desc` (:220) with `action = "change"`, and make `udev_device_get_property_value(d, "HOTPLUG")` return "1".
   - Filter by the monitor's subsystems: an input-only monitor must skip DRM records, and a DRM-only monitor must skip input records.
   - `udev_device_get_devnum` must return card0's devnum, because smithay maps a Change on a known devnum to `UdevEvent::Changed`. cosmic-comp then calls `device_changed` and scans connectors.
7. drmsmoke: add `--hotplug`. It opens `/dev/input/hotplug`, waits up to 60 s for a `drm` record, then prints `--list`. Add a test hook, ioctl 0x100B `DRM_DEBUG_FAKE_HPD`, which calls the same `hp_drm_change`. With it, QEMU and aarch64 can test the uevent path without hardware. The hook is root only.

Knobs: `i915.hpd=0` (no IRQ: the kthread polls SDEISR every 2 s), `i915.hpd_debounce_ms=100`.

Expected klog:
```
outputs: HPD irq enabled (SDEIMR 0x........ SDEIER 0x........), kthread i915-hpd
outputs: hpd DDI-B long pulse (PCH_PORT_HOTPLUG 0x........) -> reprobe   [MONITOR]
outputs: HDMI-A-1 hpd=true -> connected (2 modes, preferred 1920x1080)
outputs: uevent drm change card0 (gen 3)
```
On the shim side, set `LIBUDEV_TRACE=1` and look for `mon_recv mon=.. action=change syspath=/sys/class/drm/card0`.

Tests:
- (1) QEMU and aarch64: `drmsmoke --hotplug &` together with the fake-HPD ioctl. The record arrives and the evtest2 input hotplug still works, so input and DRM records are filtered correctly.
- (2) Laptop: the idle wakeup count with extout on is unchanged within ±1/s.
- (3) **[MONITOR]** Plug and unplug HDMI 3 times with `drmsmoke --hotplug` in a loop. Each event arrives within 1 s, and a GETCONNECTOR storm (`--list` x100) costs less than 1 ms each.
- (4) **[MONITOR]** With cosmic-comp running (still hidden per lane A), its log shows a connector scan on each plug.

---

## Lane C1: `lapdisp-crtc2` (multi-CRTC DRM core with a stub pipe). Medium. No hardware needed.

Goal: the DRM core exposes CRTC 2 with its own planes. Its backend hooks are a stub, so all of this runs in QEMU.

Object ids:
- CRTC 1 = pipe A and CRTC 2 = pipe B.
- Primary A = 30 and cursor A = 31 (existing). Add primary B = 32 and cursor B = 33.
- Plane `possible_crtcs`: A = 0b01, B = 0b10.
- The eDP encoder's `possible_crtcs` is 0b01. An extra's `possible_crtcs` is `crtc_mask(i)` (0b10 once wired).

Edits in `drm_device_interface.rs`:
- Constants at :48/:228/:230-231: add `DRM_CRTC2_ID=2`, `DRM_PLANE2_ID=32` and `DRM_CURSOR2_PLANE_ID=33`.
- `object_props` :360-430: add CRTC 2, planes 32/33, and extras for CONNECTOR.
- `current_prop_value` :7464:
  - Per-object CRTC_ID: connector 1 → CONN_CRTC, extra i → `EXT_CONN_CRTC[i]`, planes → their CRTC.
  - ACTIVE and MODE_ID per CRTC.
  - This also fixes connector 1 reporting CRTC_ID=0.
- The statics at :3462-3464 become `CRTC_ACTIVE[2]` and `CRTC_MODE_BLOB[2]`, plus `EXT_CONN_CRTC[EXT_MAX]`.
- GETRESOURCES :6827: `count_crtcs = 1 + gpu_kms::extra_crtcs()` and the id array becomes [1, 2].
- GETCRTC :7005 and SETCRTC :7132: dispatch on `crtc_id`. SETCRTC on 2 is EINVAL in v1; smithay is atomic.
- PAGE_FLIP :7256: `crtc_id` 2 goes to the pipe-B hooks.
- GETPLANERESOURCES :~7480 and GETPLANE :7500: add 32/33.
- ATOMIC :7642:
  - Fold per object into `AtomicPlaneReq` indexed by plane id, and `want_active`/`want_mode` per CRTC object.
  - A connector's CRTC_ID (connector 1 → 1 only; extra → 0 or 2 only).
  - Validate that every plane's CRTC matches its pipe, that a connector maps to at most one CRTC, and that a CRTC has at most one connector.
  - Check `changes_modeset` per CRTC (:7765).
  - Commit pipe A through the existing path unchanged, and pipe B through new hooks.
- Flip and vblank events :5041/:5229: the `drm_event_vblank.crtc_id` field must be the CRTC that flipped. Keep the per-CRTC event queues separate.
- WAIT_VBLANK: honour `_DRM_VBLANK_HIGH_CRTC_MASK` (bits 1..5) and `_DRM_VBLANK_SECONDARY` (bit 29) for CRTC index 1.

`gpu_kms.rs` seam: add `pub struct KmsPipe2` and `register_pipe2(&'static KmsPipe2)`. Keep pipe A's `KmsBackend`/`KmsFlip` untouched. It has these hooks:
- `modeset(conn_idx, &KmsTiming) -> Result`
- `disable()`
- `queue(token, sx, sy, ev) -> Result` (flip with event)
- `event(ev)`
- `busy()`
- `wait_idle(ns)`
- `cursor(...)` (optional)
- `mode()`

**Stub:** with `i915.fake` together with `i915.pipeb_stub=1`, register a `KmsPipe2` that accepts modesets and completes flip events from a 60 Hz timer (the existing synthetic-vblank code, `i915.vblank=0` path). `crtc_mask(i) = 0b10` in stub mode only.

Tests (QEMU x86_64 fake with `pipeb_stub=1`, plus aarch64 regression):
- New drmsmoke cases:
  - `CRTC_ENUM`: 2 CRTCs, and plane `possible_crtcs` are correct.
  - `ATOMIC_CRTC2`: TEST_ONLY then a real commit that binds extra 0x100 to CRTC 2 with MODE_ID and a dumb FB, then 10 page flips. Each event carries `crtc_id=2`. CRTC 1 flips keep running concurrently.
  - `ATOMIC_BAD`: binding the eDP connector to CRTC 2 → EINVAL.
- The full regress set stays green with `pipeb_stub=0`, so the default path is unchanged.

Expected log: `drm: 2 CRTCs (pipe B: stub)`, `drm: CRTC 2 modeset 1920x1080@60 conn 0x101 (HDMI-A-1)`.

---

## Lane C2: `lapdisp-pipeb-hdmi` (pipe B HDMI modeset, kernel self-test first). Large. **[MONITOR] HDMI.**

Goal: light an HDMI monitor from pipe B with a kernel test pattern, without the DRM core. Then wire it into `KmsPipe2`.

New file `drivers/src/i915/pipeb.rs`.

Prerequisites:
- **Power refcount (R8):** add `drivers/src/i915/power.rs` with `get(Dom)`/`put(Dom)` under one mutex, for `PW2` (CTL2 bits 31/30) and `DDI_B_IO` (bits 5/4), `DDI_C_IO` (7/6) and `DDI_D_IO` (9/8). Each `get` waits for the state bit, up to 1 ms. `outputs::with_pw2` switches to it.
- **DDB/WM split:** generalise `kms.rs:96 skl_ddb_plan` and `kms.rs:113 ddb_setup` to take a `[start, end)` range per pipe.
  - `ddb_setup` currently refuses when another pipe is on (:122). It should instead plan the pipe-A range as `[0, 892*wA/(wA+wB))` and the pipe-B range as the rest. This is Linux gen9's proportional `skl_ddb_get_pipe_allocation_limits`; DDB_SIZE 892 is at `kms.rs:77`.
  - Reuse the WM computation (`kms.rs:~703`, `cursor.rs:260 skl_compute_wm`) with pipe B's clock, htotal and DDB.

Sequence. This follows Linux `hsw_crtc_enable` and `intel_ddi_pre_enable_hdmi` on gen9. All offsets are pipe A + 0x1000 per pipe (`hw.rs:440 reg::pipe`, `:442 reg::plane`) unless stated.

```
0. power::get(PW2); power::get(DDI_x_IO) for the port (x = B/C per outputs.rs Port.ddi)
1. WRPLL dividers for AFE = 5 * clock_khz: port Linux intel_dpll_mgr.c skl_ddi_calculate_wrpll()
   + skl_wrpll_get_multipliers() + skl_wrpll_params_populate() verbatim (DCO 8400/9000/9600 MHz,
   +1%/-6% window, even/odd divider lists). Host unit test against Linux values for 148500, 74250,
   108000, 65000, 25175 kHz.
2. Pick DPLL1 (eDP owns DPLL0; check DPLL_CTRL2 for DDI→DPLL already in use).
   DPLL_CTRL1 0x6C058: RMW id field (shift 6*id): clear 0x3F, set OVERRIDE(bit0) | HDMI_MODE(bit5)
   DPLL1_CFGCR1 0x6C040 = FREQ_ENABLE(1<<31) | fraction<<9 | integer
   DPLL1_CFGCR2 0x6C044 = qdiv_ratio<<8 | qdiv_mode<<7 | kdiv<<5 | pdiv<<2 | central_freq
   LCPLL2_CTL 0x46014 |= 1<<31 (DPLL1 enable)   [DPLL2 = 0x46040, DPLL3 = 0x46060]
   wait DPLL_STATUS 0x6C060 bit (id*8) (DPLL_LOCK), 5 ms
3. DPLL_CTRL2 0x6C05C: clear DDI_CLK_OFF(port) (1<<(port+15)); set DDI_CLK_SEL(id,port)=id<<(port*3+1)
   | DDI_SEL_OVERRIDE(port)=1<<(port*3)   (port B=1, C=2)
4. DDI buffer: DDI_BUF_TRANS(port) = 0x64E00 + 0x60*port, entry n at +8n (lo, hi). HDMI uses entry 9.
   Value: Linux intel_ddi_buf_trans.c skl/kbl HDMI table (_skl_trans_hdmi), level from the VBT child
   byte 7 bits 4:0 (hdmi_level_shifter), else Linux's default for KBL. Copy the table, do not invent.
   If the entry has iboost: DISPIO_CR_TX_BMU_CR0 0x6C00C per Linux skl_ddi_set_iboost.
5. TRANS_CLK_SEL_B 0x46144 = (port+1)<<29
6. Timings, transcoder B: TRANS_HTOTAL_B 0x61000 = (htotal-1)<<16|(hdisp-1); HBLANK 0x61004 =
   (htotal-1)<<16|(hdisp-1); HSYNC 0x61008 = (hsync_end-1)<<16|(hsync_start-1); VTOTAL 0x6100C,
   VBLANK 0x61010, VSYNC 0x61014 likewise; PIPESRC_B 0x6101C = (w-1)<<16|(h-1)
7. WM_LINETIME_B 0x45274 = DIV_ROUND_CLOSEST(htotal*1000*8, clock_khz) (linetime in 1/8 us)
8. TRANS_DDI_FUNC_CTL_B 0x61400 = ENABLE(1<<31) | port<<28 | MODE DVI(1<<24) [v1: DVI; HDMI(0) +
   AVI infoframe is a follow-up] | BPC_8(0) | PVSYNC(1<<17 if +vsync) | PHSYNC(1<<16 if +hsync)
9. DDI_BUF_CTL(port) 0x64000+0x100*port = ENABLE(1<<31) (HDMI: translation select ignored, entry 9)
   (+ lane reversal bit 16 if VBT child flags@0x17 bit1)
10. PIPECONF/TRANSCONF_B 0x71008 = ENABLE(1<<31) | BPC_8(0<<5); wait PIPECONF_B bit30 (state), 1 ms
11. Plane 1B (0x71180 base): DDB (PLANE_BUF_CFG_1_B 0x7127C, CUR_BUF_CFG_B 0x7117C), WM levels
    PLANE_WM_1_B 0x71240..+0x1C, PLANE_WM_TRANS_1_B 0x71268, STRIDE 0x71188 (in 64 B units, linear),
    POS 0x7118C = 0, SIZE 0x71190 = (h-1)<<16|(w-1), OFFSET 0x711A4 = 0, PLANE_CTL_1_B 0x71180 =
    ENABLE | FORMAT XRGB8888 (0x4<<24) | linear, PLANE_SURF_1_B 0x7119C = GGTT address (arms)
12. Pipe B interrupts: DE_PIPE_B IMR/IER 0x44414/0x4441C unmask VBLANK(bit0) + PLANE1_FLIP_DONE(bit3);
    ISR vblank.rs:466-475: route MASTER_DE_PIPE_B IIR (0x44418) to a pipe-B vblank/flip handler
    instead of ack-only; FIFO underrun (bit31) counted per pipe.
Disable (reverse): plane off + SURF (wait vblank B) → PIPECONF_B off, wait state clear (≤50 ms) →
TRANS_DDI_FUNC_CTL_B = 0 → DDI_BUF_CTL off, wait DDI_BUF_IDLE (bit7) 8 µs+ → TRANS_CLK_SEL_B = 0 →
DPLL_CTRL2 set DDI_CLK_OFF(port) → LCPLL2_CTL off → DDB: grow pipe A back → power::put.
```

DDB transition: shrink A first. That means a new `PLANE_BUF_CFG_1_A` plus new WM, armed by A's `PLANE_SURF` (`kms.rs:338`), and a wait for A's vblank. Only then enable B. On disable, turn B's plane off and wait for B's vblank before growing A. The new ranges must never overlap a live pipe's old range.

The self-test knob `i915.pipeb_test=hdmi1|hdmi2` (default off):
- After `kms::init`, if that connector is connected, allocate a GGTT-backed FB (reuse the `kms.rs:262 ggtt_of` slot window) and fill it with colour bars and a moving bar.
- Run the sequence with the preferred mode (after `mode_ok`). Hold it for 10 s, then disable.
- Log the frame counter (`PIPE_FRMCOUNT_B`) at +1 s and +10 s, and the underruns.

Then wire it into `KmsPipe2` (lane C1 hooks). Flips reuse the pipe-A flip machinery parameterised by pipe; `vblank.rs` gets per-pipe queues.

Knobs: `i915.pipeb_test`, `i915.hdmi_dvi=1` (the v1 default), `i915.pipeb_dpll=1|2|3`.

Expected klog **[MONITOR]**:
```
pipeb: HDMI-A-1 1920x1080@60 148500 kHz (DDI-B): WRPLL dco 8910000 kHz p0 2 p1 1 p2 5 cfgcr1 0x........ cfgcr2 0x........ -> DPLL1 locked in N us
pipeb: DDI-B buf trans entry 9 = 0x......../0x........ iboost 0; TRANS_CLK_SEL_B 0x40000000
pipeb: DDB A [0,446) B [446,892); WM B lvl0 0x........; linetime 59
pipeb: TRANSCONF_B on; frame counter 0 -> 60 in 1 s, 600 in 10 s; underruns A 0 B 0
pipeb: test pattern held 10 s, disabled; DDB A [0,892) restored
```
The `kms: underruns` line on pipe A stays 0 throughout. That is the DDB-transition check.

Tests:
- QEMU fake: the pipeb code paths are compiled in but `pipeb_test` is skipped with `fake`. aarch64 regression boot.
- **[MONITOR]** laptop, HDMI 1080p:
  - The user confirms the colour bars, and the eDP desktop never glitches.
  - Repeat with the monitor at 1280x720 by forcing `i915.pipeb_mode=1280x720`.
  - Repeat with a 4K TV if one is available. It must pick a mode ≤300 MHz.
- Unplug during the test: there must be no hang, and the disable sequence still completes.

---

## Lane C3: `lapdisp-dp` (DP link training and DP modeset on pipe B). Large. **[MONITOR] USB-C DP or DP++ DP.**

Steps 0, 3, 5-7 and 10-12 are the same as C2. The DP-specific parts follow Linux `intel_ddi_pre_enable_dp`, `intel_dp_start_link_train` and `drm_dp_helper`:

```
a. Sink caps (probe already reads DPCD 0x000..0x00F): rev 0x000, MAX_LINK_RATE 0x001 (06/0a/14),
   MAX_LANE_COUNT 0x002 (bits 4:0, bit7 enhanced framing, bit6 TPS3), MAX_DOWNSPREAD 0x003 bit0,
   TRAINING_AUX_RD_INTERVAL 0x00E (0 → 100 µs CR / 400 µs EQ, else n*4 ms).
b. Pick lowest (rate, lanes) with clock*3 ≤ link_khz*lanes (24 bpp); knobs can force.
c. DPLL (DP mode, no CFGCR): DPLL_CTRL1 id field = OVERRIDE | LINK_RATE<<1 where code
   2700 MHz(HBR2 5.4)=0, 1350(HBR 2.7)=1, 810(RBR 1.62)=2 (+SSC bit4 = 0); enable LCPLL2_CTL; lock.
   DPLL_CTRL2 mapping as C2.3.
d. DDI buf trans: all 9 DP entries (0..8) from Linux kbl U-SKU DP table (_kbl_u_trans_dp; device id
   says ULT/ULX) — copy the table. Level index for (vswing v, pre-emph p):
   (0,0)=0 (0,1)=1 (0,2)=2 (0,3)=3 (1,0)=4 (1,1)=5 (1,2)=6 (2,0)=7 (2,1)=8 (3,0)=9→clamp to 8.
e. Sink power: DPCD 0x600 SET_POWER = 1 (D0), retry 3x with 1 ms.
f. DPCD 0x100 LINK_BW_SET = rate code; 0x101 LANE_COUNT_SET = lanes | 0x80 if enhanced;
   0x107 DOWNSPREAD_CTRL = 0; 0x108 MAIN_LINK_CHANNEL_CODING_SET = 1 (8b10b).
g. TRANS_CLK_SEL_B as C2.5. DP_TP_CTL(port) 0x64040+0x100*port = ENABLE(1<<31) | SST(0) |
   ENHANCED_FRAME(1<<18 if enhanced) | PAT1(0<<8). DDI_BUF_CTL(port) = ENABLE | PORT_WIDTH
   ((lanes-1)<<1) | TRANS_SELECT(level<<24) (+lane reversal); posting read; udelay(600).
h. Clock recovery: DPCD 0x102 TRAINING_PATTERN_SET = 0x21 (TPS1|SCRAMBLING_DISABLE) and
   0x103..0x103+lanes-1 TRAINING_LANEx_SET = v | p<<3 | (v==3?0x04) | (p==3?0x20) (max-reached).
   Loop: wait CR interval; read 0x202..0x207 (LANE0_1_STATUS, LANE2_3_STATUS, ALIGN, SINK_STATUS,
   ADJUST_REQUEST_LANE0_1, ADJUST_REQUEST_LANE2_3). CR_DONE = bit0 of each lane nibble. If all → EQ.
   Else v/p = max requested over lanes; if v unchanged 5 times or 10 loops → fail (try lower rate,
   then fewer lanes: Linux intel_dp_get_link_train_fallback_values). Each adjust: rewrite
   DDI_BUF_CTL TRANS_SELECT(level) (+ wait 1 µs) and 0x103.. lane set.
i. Channel EQ: DP_TP_CTL pattern = PAT2 (1<<8) or PAT3 (4<<8) if TPS3 && HBR2; DPCD 0x102 = 0x22/0x23.
   Loop ≤5: wait EQ interval; read status; need per lane CR_DONE|CHANNEL_EQ_DONE(bit1)|
   SYMBOL_LOCKED(bit2) and 0x204 INTERLANE_ALIGN_DONE(bit0). CR lost → back to h. Adjust as h.
j. DP_TP_CTL pattern = IDLE (2<<8); wait DP_TP_STATUS(port) 0x64044+0x100*port bit25 IDLE_DONE, 1 ms.
k. M/N (Linux intel_link_compute_m_n, constant N): data_m = clock*3, data_n = link_khz*lanes;
   link_m = clock, link_n = link_khz; scale each pair to N = 0x8000 (m = m*0x8000/n), keep ≤ 0xFFFFFF.
   PIPE_DATA_M1_B 0x61030 = TU(64-1)<<25 | data_m; PIPE_DATA_N1_B 0x61034; PIPE_LINK_M1_B 0x61040;
   PIPE_LINK_N1_B 0x61044. TRANS_MSA_MISC_B 0x61410 = SYNC_CLK(1) | 8BPC(1<<5).
l. Timings/PIPESRC/linetime as C2.6-7; TRANS_DDI_FUNC_CTL_B = ENABLE | port<<28 | MODE DP_SST(2<<24)
   | BPC_8 | polarity | (lanes-1)<<1; PIPECONF_B on; plane 1B as C2.11.
m. After the first vblank: DP_TP_CTL pattern = NORMAL (3<<8); DPCD 0x102 = 0.
n. Short-pulse HPD (lane B): read 0x202..0x204; if !(CR|EQ|align) on a lit pipe → retrain (h..m,
   pipe stays on) and log; persistent failure → set connector "link-status" BAD (C4).
Disable: DP_TP_CTL off after DDI_BUF_CTL off + idle wait; DPCD 0x600 = 2 (D3); rest as C2.
```

Self-test knob: `i915.pipeb_test=dp1|dp2`, with the same pattern and the same 10 s hold as C2.

Knobs: `i915.dp_rate=162000|270000|540000`, `i915.dp_lanes=1|2|4`, `i915.dp_tps3=0`.

Expected klog **[MONITOR]**:
```
dp: AUX-C DPCD rev 0x12, max 4 x 5.4 GHz (HBR2), enhanced, TPS3, aux_rd_interval 0
dp: DP-2 1920x1080@60 148500 kHz -> 2 x HBR (data M/N 0x......../0x8000 link M/N 0x......../0x8000)
dp: CR ok after 1 loop (v0 p0); EQ ok after 2 loops (v1 p0) TPS2; status 0x77 0x77 align 0x81
pipeb: TRANSCONF_B on (DP-2); frame counter 0 -> 60 in 1 s; underruns 0; DP_TP_CTL normal
```

Tests **[MONITOR]**:
- The USB-C DP adapter or monitor, and a DP sink on the DP++ port if the user has a DP cable. The note does not say which jack is which, so record it.
- Force `dp_lanes=1` and `dp_rate=162000` to exercise the fallback path.
- Unplug while lit; replug.
- QEMU fake: compiled in, skipped. aarch64 regression boot.

---

## Lane C4: `lapdisp-extend` (cosmic-comp end to end). Medium. **[MONITOR]**

1. Wire `crtc_mask(i) = 0b10` for real hardware once C2 or C3 is merged. Lane A's "hidden" rule then exposes the connector automatically.
2. Connector properties for extras and eDP, in `object_props` :360 and `current_prop_value` :7464:
   - `EDID` (blob from the probe; cosmic-comp uses it for make, model and config keying)
   - `non-desktop` = 0 (silences `device.rs:~1045`)
   - `link-status` (GOOD/BAD enum, set by C3.n)
   - `DPMS`
3. Teardown on unplug while lit: keep the pipe running until the compositor's commit sets CRTC_ID=0 (Linux behaviour). For DP, set link-status BAD. If no client is master, disable after 2 s.
4. Lid: cosmic-comp `state.rs:841` engages with 2 or more outputs and needs logind's inhibit fd. Check that the call fails gracefully on LeandrOS (no logind) and that closing the lid with an external monitor does not blank both outputs.
5. Flip `i915.pipeb` to default on, only after the test below passes 3 times.

Tests **[MONITOR]**:
- Boot with HDMI plugged in, log in: the desktop extends.
- Run `cosmic-randr list`, or check cosmic-settings → Displays, which shows both outputs. Mirror, then extend.
- Unplug and replug 5 times while logged in. There is no compositor crash, and `drmsmoke --list` (run as root over SSH) agrees.
- Play YouTube on the external output for 5 min: underruns on A and B are 0.
- Run the same steps with USB-C DP.
- Measure with the screen lit (laptop perf wave rule).

---

## Size and risk summary
| Lane | Size | Hardware needed | Main risk |
|---|---|---|---|
| A | S | laptop, monitor for check (3) | none if the hidden rule is applied |
| B | M | monitor for (3), (4) | shim filter mixing input and DRM records |
| C1 | M | none (stub) | atomic state per CRTC; pipe-A path must stay byte-identical |
| C2 | L | HDMI monitor | DDB transition underruns on eDP; WRPLL port errors |
| C3 | L | DP or USB-C monitor | link-training corner cases; USB-C alt-mode mux owned by the PD/EC firmware |
| C4 | M | monitor | cosmic-comp config edge cases |

Reference ports, copied rather than reinvented:
- `skl_ddi_calculate_wrpll` (intel_dpll_mgr.c)
- `_kbl_u_trans_dp` and `_skl_trans_hdmi` (intel_ddi_buf_trans.c)
- `skl_ddb_get_pipe_allocation_limits` (gen9 proportional split)
- `intel_link_compute_m_n`
- `intel_dp_link_training.c`

The register offsets and bit positions above are from the SKL/KBL PRM vol 2c as mirrored in Linux `i915_reg.h`. Before writing, cross-check each one against `drivers/src/i915/hw.rs`, where 400-442 already holds the pipe-A forms.
