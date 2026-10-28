# Review: lane/lapdisp (merge 2bf33814), 2026-10-27

Scope: `git diff 2bf33814^1 2bf33814`, which adds `drivers/src/i915/outputs.rs`, the `gpu_kms.rs` seam, the `drm_device_interface.rs` extras, and drmsmoke CONNECTOR_ENUM. I cross-checked against the cosmic-comp sources in `~/code/cosmic-epoch/cosmic-comp` and the Linux i915 code as I remember it.

**Verdict.** The default-off path is truly unchanged, and it is safe to keep merged. **Do NOT flip `i915.extout=1` to default** as the lane note plans ("boot with extout=1, greeter comes up, flip"). The real breakage happens only when a monitor is plugged in (R1), and the no-monitor trial cannot see it. There is also one kernel-panic path (R2).

## Findings (by severity)

### R1 HIGH: a connected extra with possible_crtcs 0 breaks cosmic-comp's output config
- cosmic-comp `backend/kms/drm_helpers.rs:50-72` maps every `Connected` connector, with `None` as its CRTC when no encoder offers one. `device.rs:1027 connector_added` then creates an `Output` for it with a default (enabled) config.
- `backend/kms/mod.rs:~808-853 apply_config_for_outputs` collects the enabled outputs that have no surface. For a connector without a CRTC it runs `anyhow::bail!("Missing crtc for {conn:?} ...")`.
- `config/mod.rs:468-501` then retries the known-good config. That config also has the extra enabled, so the retry fails the same way and returns `"Failed to reset config"`.
- Result: no output config is applied, including eDP's. The likely outcome is a black or unscaled panel, or a failed session start, whenever an HDMI or DP monitor is plugged in at boot, at login, or later.
- **Fix (lane A):** report `connection = disconnected` to DRM clients for any extra whose encoder has no possible CRTC. Keep the true status in klog and `/proc`, and behind a debug knob.

### R2 HIGH (kernel panic): a partial I2C-over-AUX reply overflows the EDID slice
`outputs.rs:339-344`: `aux_req(.., READ_MOT, .., 16, ..)` may legitimately return fewer than 16 bytes, because docks, MST hubs and DP++ converters send partial ACKs. `off` then stops being a multiple of 16, so `edid[off..off+got]` runs past 128 and panics on the GETCONNECTOR ioctl path.

Fix: request `min(16, 128-off)` and copy `min(got, 128-off)`, as Linux's `drm_dp_i2c_drain_msg` does.

### R3 MED: VBT child-device parsing
- `outputs.rs:102` reads the AUX channel from `c[0x1a]`. In `struct child_device_config` the layout is dvo_cfg@0x16, flags@0x17, support bits@0x18, **aux_channel@0x19**, dongle_detect@0x1a. The code is reading `dongle_detect`, which is usually 0, so it falls back to `_ => ddi`. It is correct on this laptop by accident and wrong on any board that remaps AUX. Fix: `c[0x19]`.
- `outputs.rs:98` matches only `7|8|9` (DVO_PORT_DPB..DPD), although the comment says "also HDMIB..D". HDMI-only ports use `DVO_PORT_HDMIB..D = 1..3`, so they are dropped. Fix: `1|7 => B, 2|8 => C, 3|9 => D`. Linux also maps 11/12 (DPE/HDMIE) to DDI-E; that can be skipped on KBL-U.

### R4 MED: a failed probe sticks until the next replug
- `outputs.rs:209-212` stores `hpd` even when the AUX or GMBUS probe failed. Monitors commonly raise HPD 100 to 500 ms before their EDID or DPCD answers, so the cached result can stay "disconnected" forever.
- If a sink ACKs with an invalid EDID, the result is "connected, 0 modes" (`outputs.rs:205-206`). cosmic-comp `populate_modes` (device.rs:1086) then fails.
- Fix:
  - Store `hpd` only after a probe that gave a clean answer. Otherwise retry on later calls with backoff (at least 500 ms apart, at most 5 tries).
  - For connected-without-EDID, add the Linux `drm_add_modes_noedid` fallback modes (1024x768, 800x600 and 640x480 at 60 Hz).

### R5 MED: PCH hotplug detection is never enabled (unverified)
- The SDEISR live bits for DDI B..D (bits 21..23) only work when `PCH_PORT_HOTPLUG` (0xC4030) has `PORTB/C/D_HOTPLUG_ENABLE` set (bits 4/12/20). Linux sets these in `spt_hpd_detection_setup`.
- We rely on the BIOS. The laptop log shows SDEISR=0x01000000 (port A only), which proves nothing about B..D.
- Fix: log `PCH_PORT_HOTPLUG` at init and set the enable bits. The status bits [1:0]/[9:8]/[17:16] are write-1-to-clear, so mask them out of the read-modify-write.

### R6 MED (needed before any modeset): no mode validation
- EDID modes are listed without checking hardware limits:
  - KBL HDMI TMDS is at most 300 MHz.
  - DP bandwidth is `clock*3 ≤ link_khz*lanes`.
  - Pixel clock must be ≤ the current CDCLK (gen9 has no 2 px/clk).
- A 4K60 TV DTD at 594 MHz would be offered to the compositor.
- Other EDID gaps:
  - Interlaced DTDs (byte 17 bit 7) are not flagged or skipped.
  - The EDID checksum is not verified.
  - The CEA extension (block 1) is ignored. Many TVs put most of their modes there.

### R7 LOW-MED: GMBUS details
- It is missing Display WA #0868 for SPT/KBP: set `SOUTH_DSPCLK_GATE_D` (0xC2020) bit 31 `PCH_GMBUSUNIT_CLOCK_GATE_DISABLE` for the duration of the transfer and restore it afterwards. Without it, HDMI EDID reads can fail intermittently.
- `outputs.rs:364`: `m.wait(GMBUS2, HW_RDY|SATOER, HW_RDY, 50_000)` never matches on a NAK, because SATOER is set and HW_RDY is clear. Every NAK therefore spins for the full 50 ms before it is classified. A sink that misbehaves mid-read can spin 32×50 ms = 1.6 s inside the ioctl. Fix: poll and exit on either bit.

### R8 LOW-MED: `with_pw2` is not a refcount
`outputs.rs:258-269` read-modify-writes `PWR_WELL_CTL2` with no lock, and drops the request whenever the request was clear on entry. That is harmless today. Once pipe B (or anything else that needs PW2) exists, a concurrent GETCONNECTOR probe would turn PW2 off under a lit pipe. Replace it with a refcounted `power::get(Domain)/put` under one mutex before lane C3.

### R9 LOW: GETCONNECTOR re-probe cost and locking (verified OK, with caveats)
- GETCONNECTOR is dispatched with no DRM lock (`drm_device_interface.rs:6188`), and `info()` drops CONNS/PORTS before probing. The lock order CONNS→PORTS is consistent.
- GETRESOURCES calls `ext_count()` (CONNS spinlock) under the DRM spinlock (6827). That is fine, provided **no IRQ path ever takes CONNS**. That is a constraint for lane B.
- Cost while HPD is unchanged: 1 MMIO read plus 2 spinlocks, which is cheap.
- On an HPD change, probing costs: DP about 1 to 5 ms; HDMI on DP++ about 6 ms (AUX timeout) plus about 13 ms (GMBUS 128 B at 100 kHz). The worst case is about 1.6 s (R7). All of it busy-spins in the compositor's ioctl.
- Lane B moves probing into a kthread so GETCONNECTOR only reads the cache.
- `init` holds the PORTS lock across the boot AUX probes (`outputs.rs:147`, the for-loop temporary). This is harmless before `register_outputs`, but should be cloned first.

### R10 LOW: the eDP connector-type change
- `drm_device_interface.rs:6876`: connector 1 is eDP (14) only while extras are registered, otherwise HDMI-A (11). The name the compositor sees therefore flips with a kernel knob.
- cosmic-comp names outputs from the interface (`drm_helpers.rs:185`). Saved output configs keyed by "HDMI-A-1" (in the user's and the greeter's cosmic-comp state) are no longer matched, so scale and mode reset once.
- eDP also changes behaviour:
  - Scaling goes through the "laptop" DPI path (`device.rs:1371`). With the synthetic 100-DPI mm (6898) the result is still 1.0, so there is no visible change.
  - Lid handling (`state.rs:841`) engages only with 2 or more outputs, so it stays inert until pipe B exists.
- Recommendation: decide the type once, independent of extout. Report eDP whenever the i915 backend scans out through `TRANS_DDI_FUNC_CTL_EDP` (`kms.rs:653`). The name is then stable across knob flips. Later, report the panel's real mm from the eDP EDID (AUX-A).

### R11 LOW: smaller notes
- The main encoder reports `encoder_type = 3`. That is LVDS, not VIRTUAL (VIRTUAL is 5); the comment at 6999 is wrong and the issue predates this lane. For eDP, report TMDS (2) like Linux.
- Extras report `count_props = 0` in GETCONNECTOR, while OBJ_GETPROPERTIES returns CRTC_ID. smithay uses the latter, so this works. There is no `EDID` blob property, so cosmic-comp reports make/model "Unknown". Add EDID, `non-desktop`=0 and `link-status` in C4.
- Id space: connectors 0x100+ and encoders 0x180+ do not collide with props (40..70), planes (30/31), blob ids (0x1000+) or crtc/conn/enc 1.
- Atomic: CRTC_ID=0 on an extra is accepted and anything else gets EINVAL (7711). smithay's reset commit works. cosmic-comp never targets an extra because it finds no CRTC.

## Default-off path: verified unchanged
- `ext_count()` is 0 when nothing is registered. GETRESOURCES compares `count >= 1 + 0` and writes the same arrays and counts.
- In `object_props`, GETCONNECTOR, GETENCODER and the atomic CONN_CRTC_ID arm, the new guards call `ext_index(..)` → `None` and fall through to the old arms unchanged. Connector 1 stays type 11.
- `outputs::set_vbt` now runs on every boot (`probe.rs:350`). It is bounds-checked: every read goes through `get()` and `chunks_exact`, `sz=0` still advances the loop, and I found no panic path. It produces at most one extra klog line, plus the "not reported" line from `init`.
- drmsmoke's default run gains one `CONNECTOR_ENUM` case. Check whether any regress expectation counts drmsmoke cases.

## Must-fix before default-on
R1, R2, R4 (the retry part), and R10 (stable type). R3, R5 and R7 are cheap and belong in the same lane.
