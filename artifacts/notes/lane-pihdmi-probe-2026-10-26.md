# Lane pihdmi-probe (2026-10-26): read-only HDMI0/1 boot probe on the Pi 5 (BCM2712 D0)

Status: DELIVERED (code + host tests; the hardware answer comes with the next Pi boot).
Lane 1 of artifacts/notes/lane-pisound-design-2026-10-26.md.

## What it does
`drivers/src/hdmi_probe.rs` (rpi5 only, called from `kernel/src/init.rs` right after the v3d
probe, bracketed by `serror_checkpoint()` like pv3dpwr2). `hdmi.probe=0` disables it.
- NO writes to any display register. The only writes are the mailbox property calls
  (read-only tags: GET_CLOCK_RATE 9/13/14, GET_EDID_BLOCK, GET_NUM_DISPLAYS).
- dvp (0x10_7c70_0000) is read first; then, per port, one pass over the `hdmi` block
  (0x10_7c70_1400 / 0x10_7c70_6400) and the shared `hd` block (0x10_7c72_0000), then a checkpoint.
  An SError logs `[hdmi] hdmiN serror syn=..` and skips the port (PROBE_STATE = 4).
- Frame rate: wait for a FRAME_COUNT edge (<= 100 ms), count edges until >= 100 ms (<= 300 ms
  total), timed edge to edge, so 59.94 vs 60 is resolvable. Pixel clock = htotal x vtotal x fps,
  snapped within 0.5 % to the CEA ACR clocks (incl. /1.001) or a DMT list, else `raw`.
- Boot cost: <= ~0.6 s worst case (two ports x 300 ms) when nothing scans out; ~0.2 s normally.

## Lines (tag `[hdmi]`, per orchestrator; the design note said `[hdmiaud]`)
```
[hdmi] dvp sw_init=0x.. misc=0x..
[hdmi] hdmi0 hpd=1 sched=0x.. hdmi_mode=1 active=1 gate=open rst_bit=.. mai_ctl=0x..(en=0 ch=0 errE=0 errF=0) ...
       crp=0x..(n=.. ext=.. dis=..) cts=.. apc=.. ram_pkt=0x..(en=1 mask=0x0004) ... h=1920/2200 v=1080/1125
       fps=60.00 pixclk=148500 kHz (cea raw=1485xx) n48=6144 cts48=148500
[hdmi] hdmi1 hpd=0 ... fps=0.00 pixclk=0 kHz (raw raw=0) n48=0 cts48=0
[hdmi] mbox clk9=.. clk13=.. clk14=.. kHz edid=<n>blk sink=hdmi|dvi|refused fkms=<n>|refused (measured hdmi0=.. hdmi1=.. kHz)
[dma40] slots@0x1000010600 cs=........ x6 active=0
```

## Evidence
- `cd drivers/hdmi-probe-tests && cargo test --target aarch64-apple-darwin`: 6/6 (timing decode
  of 1080p/720p/VGA/4k encoded exactly as Linux `vc5_hdmi_set_timings`; fps + CEA/DMT snapping
  incl. 59.94; the full HDMI 1.4b tables 7-1..7-3 + HDMI 2.0 594 MHz N/CTS (35 rows); DVP/MAI/CRP
  decoders; EDID HDMI-VSDB detection). With the gen-4 HFP shift and a wrong 297 MHz N row: 3 fail.
- `scripts/check-rpi5.sh` and `CHECK_FEATURE=raspi4b`: clean, no warnings in touched files.
- aarch64 virt boot + suites: see the merge report (the probe is not compiled for virt).

## Open / for later lanes
- **DREQ correction for lane 5:** this DTB has `dmas = <&dma40 0x41fa000a>` (hdmi0) and
  `0x41fa0011` (hdmi1), i.e. PER_MAP **10 / 17**, not 12 / 13 as the design note says.
- DVP SW_INIT polarity is unverified for BCM2712 (reset-simple, maybe active-low): the line
  shows the raw bit (`rst_bit`), not an interpretation. Gate = MISC_CONFIG bit 3+N set means closed.
- FRAME_COUNT width is assumed 32-bit (wrapping delta); interlaced and pixel-repeated modes are
  not decoded (vtotal would read as one field).
- dma40 channel numbering inside the 0x600 window (slot k = ch 6+k?) is lane pidma40's to settle.
