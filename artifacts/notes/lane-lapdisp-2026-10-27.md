# Lane lapdisp (2026-10-27): PARTIAL (detection and reporting done, no second-pipe modeset)

Branch lane/lapdisp: fdc4161a (feature) plus a merge of main.

## What landed
- `drivers/src/i915/outputs.rs` (new): reads the external DDI ports from the VBT child devices (block 2: dvo_port, DDC pin, AUX channel, device type). If the VBT is missing, it falls back to SFUSE_STRAP. A DP++ (dual-mode) port becomes a DP and an HDMI-A connector, as on Linux. Other parts:
  - live HPD from SDEISR (SPT bits 21..23 = DDI B..D);
  - native DP AUX (DP_AUX_CH_CTL/DATA at 0x64010 + ch*0x100, power well 2 held for the transaction, DEFER/timeout retries);
  - EDID over I2C-over-AUX for DP, and over GMBUS for HDMI;
  - EDID detailed timings become DRM modes, and the EDID gives the size in mm.

  A connector is re-probed on GETCONNECTOR when its live HPD bit changes; no DRM lock is held there. Every other path reads the cache.
- `gpu_kms.rs`: `KmsOutputs` seam. The extra connectors have ids 0x100+i, their encoders 0x180+i (TMDS type, possible_crtcs 0, unbound).
- `drm_device_interface.rs`:
  - GETRESOURCES lists the extras.
  - GETCONNECTOR and GETENCODER answer for them.
  - The extras expose the CRTC_ID property. An atomic commit may set it only to 0, so smithay's reset still works.
  - Connector 1 is reported as eDP (14) while extras are registered. It used to be type 11 = HDMI-A, not VIRTUAL; the old comment was wrong. Without extras it stays 11.
- drmsmoke: a `CONNECTOR_ENUM` case: every connector and encoder listed must answer. `drmsmoke --list` prints one line per connector.
- Knobs: **default OFF**, `i915.extout=1` enables it. Kept off because a second trial meant to check that cosmic-comp tolerates the extras (and connector 1 as eDP) found the laptop unreachable ("Host is down") before the deploy started, so compositor health with the extras is unverified; `i915.extout_aux=0` skips the boot AUX probe.

## Evidence
- Real laptop, trial 268ed10458ad (77aa5216). The VBT has DDI-B (GMBUS pin 5, AUX B) and DDI-C (pin 4, AUX C), both DP++. SDEISR=0x01000000: only eDP is live.
  - AUX-B and AUX-C each time out cleanly in 5.8 ms (3 tries). The channel never stays busy, so the register path works.
  - `drmsmoke --list`: eDP-1 connected 1920x1080@60. DP-1, HDMI-A-1, DP-2 and HDMI-A-2 all report disconnected, possible_crtcs 0. CONNECTOR_ENUM PASS.
  - The laptop was rebooted back to good afterwards; the trial was not promoted.
- x86_64 QEMU, `i915.fake=2 i915.kms=1`: same 4 extras disconnected, CONNECTOR_ENUM PASS. drmsmoke's 3 other failures are fake-mode cases unrelated to this lane (SYNCOBJ_TIMELINE, V3D name).

## Open (next lanes)
0. On the laptop, boot with `i915.extout=1` and check that cosmic-greeter/cosmic-comp come up (`ps`) and log in. If clean, flip the default to on.
1. Second-pipe modeset on pipe B: transcoder B, DPLL1/2 (DPLL_CTRL1/2), DDI_BUF_CTL/DDI_FUNC_CTL, DDI buffer translations, plane 1B, DDB/WM split across 2 pipes. DP link training (clock recovery, then channel EQ, with DPCD 0x100..0x10x and DP_TP_CTL). HDMI TMDS mode. Then possible_crtcs = 2 and a second CRTC/plane in the DRM core. The DRM core has a single CRTC/plane today, so it needs a multi-CRTC object model.
2. HPD interrupt (SDEIIR) and a DRM hotplug uevent, so cosmic-comp rescans on plug. Today status is polled on GETCONNECTOR.
3. Which physical port is DDI-B and which is DDI-C (HDMI jack vs USB-C/TB DP-in) is unknown until a monitor is plugged in. Both are DP++ in the VBT.
4. Real-monitor test: plug in HDMI, then USB-C DP. Expect `connected` plus EDID modes in `drmsmoke --list` and the `outputs:` klog line.
