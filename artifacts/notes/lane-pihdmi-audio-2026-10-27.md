# Lane pihdmi-audio (2026-10-27): HDMI audio output on the Pi 5 (BCM2712 D0, hdmi0)

Status: DELIVERED (host-tested and compile-checked; audible output is unproven until the next Pi boot).
Lane 5 of `lane-pisound-design-2026-10-26.md`. Builds on pihdmi-probe (pixel clock, HDMI/DVI) and pidma40.

## What landed
- `drivers/src/hdmi_audio/pure.rs` (every arch): IEC958 subframes exactly as alsa-lib `pcm_iec958.c`
  (audio in bits 4..27, S16 in 12..27, C bit = channel status bit `frame % 192`, even parity over 4..30,
  preambles Z/X/Y = 0x08/0x02/0x04, matching `B_FRAME_IDENTIFIER = 8`), consumer channel status (PCM,
  fs code in byte 3, word length + original fs in byte 4), the Linux gen-6 register images (MAI_CTL
  startup/run, MAI_THR with the **D0 `VC6_D` 7-bit layout 23/15/7/0** vs C0/C1 gen-5 24/16/8/0, MAI_FMT,
  MAI_SMP from `rational_best_approximation(108 MHz, fs)` = 2250/1 at 48 kHz and 120000/49 at 44.1 kHz,
  MAI_CONFIG, the vc5 channel map, AUDIO_PACKET_CONFIG, CRP_CFG), the audio InfoFrame + the packet-RAM word
  layout of `vc4_hdmi_write_infoframe` (slot 4, stride 0x24), ring bookkeeping and S16 conversion
  (mono duplication, linear resampling of other rates to 48 kHz).
- `drivers/src/hdmi_audio/mod.rs` (rpi5): `HdmiAudio` with the seven PipeWire backend calls. The ring is
  64 IEC958 blocks (98304 B, 8 periods of 12288 B, 512 ms at 48 kHz). Because the ring is a whole
  number of 192-frame blocks, a ring offset fixes the frame's block position, so consumed regions are
  refilled with *valid* IEC958 silence (correct preambles/C/P) from one template, not zeros. Native rates
  48 kHz and 44.1 kHz (MAI_SMP/MAI_FMT/N/CTS/channel status reprogrammed per SET_PARAMS); others resample.
- Bring-up order (`probe`): probe verdict (hdmi0 hpd or active, HDMI mode, pixel clock known) -> map
  dvp/hd/hdmi0/packet RAM -> open the DVP audio gate if closed (MISC_CONFIG bit 3; SW_INIT is never
  touched) -> buddy ring + dma40 `claim(Some(10))` -> MAI startup + prepare + ACR (N/CTS from the probe's
  spec table, Linux formula fallback) + InfoFrame (RMW, AVI slot preserved) -> cyclic DMA (DREQ cell
  0x41fa000a) into MAI_DATA at CPU 0x10_7c72_001c, verified by the position moving within 40 ms, else
  retried at bus 0x7c72001c, else inert. Every step is SError-checkpointed (`set_serror_check`).
- Runtime: MAI ERRORE/ERRORF/DLATE counted and cleared; a firmware re-train (MAI_CTL.ENABLE dropped or
  CRP_CFG N changed) or 30 ticks without DMA progress reprograms and restarts (bounded logs); writer
  starvation resyncs as HDA does (`writer starved #n`).
- `hdmi_probe.rs` now exports `PORT_FLAGS`, `PORT_PIX_KHZ`, `EDID_SINK`.
- `servers/pipewire`: one new `Backend::Hdmi` variant (tried after virtio and HDA, `snd.backend=hdmi`
  pins it) and `is_initialized()`. `kernel/src/init.rs` (rpi5 only) retries `pipewire_server::init`
  after the HDMI probe + dma40 self-test, because PipeWire's first init runs before them.
  The virtio-sound and HDA paths are untouched.
- `hdmi.audio=0` disables it. The tone check is the existing `/bin/sndtest`.

## Evidence
- `cd drivers/hdmi-audio-tests && cargo test --target aarch64-apple-darwin`: 8/8 (subframes vs a
  transcription of alsa-lib `iec958_subframe` on 5000 random S16/S24 samples, plus literal 0x81234568;
  channel status bytes and C-bit positions; register images incl. both MAI_THR layouts and MAI_SMP at
  48/44.1 kHz; N/CTS 148.5 MHz -> 6144/148500 and 6272/165000; InfoFrame bytes + checksum + packet words;
  ring geometry through the real dma40 `build_cyclic_tx` for both FIFO addresses; advance/wrap/refill/
  underrun; conversion). Mutation check: gen-5 THR on D0, parity over bit 31, misplaced Z preamble -> 3 fail.
- `scripts/check-rpi5.sh` and `CHECK_FEATURE=raspi4b`: clean, no warnings in touched files.
- aarch64 virt regress (HVF, Mac, tag pihdmiaud): overall PASS, boots failed 0/1, all listed suites RC=0 (virtio-sound/HDA paths unchanged).

## Expected next Pi boot (HDMI screen with speakers on hdmi0)
- `[hdmiaud] ready: hdmi0 pix=148500 kHz N=6144 CTS=148500 gate=open|opened thr=d0 dma ch10 fifo=cpu|bus(0x..) irq|poll edid=hdmi`
  then `[PW] playback device: HDMI audio (hdmi0)` and `[INIT] PipeWire server port (HDMI audio): N`.
- Test: `sndtest info` (shows `backend: hdmi`, counters), `sndtest tone 440 5` (5 s 440 Hz tone), `sndtest all`.
- Failure lines: `[hdmiaud] inert: <reason>` (no sink / DVI / unknown clock / dma never moved). On
  `no progress pos=.. cs=.. debug=.. mai_ctl=..` for both FIFOs, DREQ 10 never asserted: check MAI_CTL
  and the dma40 DEBUG bits.

## Open
- Unproven on hardware: DREQ pacing, which FIFO address works, whether the firmware keeps our packet RAM
  slot/CRP across a hotplug (the re-train check covers it), and the preamble choice if the sink stays silent
  (alsa's vc4-hdmi.conf historically overrode preambles; ours match B_FRAME_IDENTIFIER = 8 as Linux sets it).
- hdmi1 and >2 channels are not wired (offsets differ: MAI_CTL/DATA/SMP at HD+0x30/0x3c/0x40, DREQ 17).
