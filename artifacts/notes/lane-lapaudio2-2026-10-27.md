# lane lapaudio2 — 2026-10-27: HDA capture, jack sense, display codec

Status: **mic capture FIXED (real laptop), jack sense PARTIAL (sense read, switching untested — nothing plugged), HDMI/DP PARTIAL (codec enumerated, no playback).**

## What changed
- `drivers/src/hda_capture.rs` (new, child module of `hda.rs`):
  - Capture: ADC → mixer/selector → mic-pin paths found by DFS over connection lists; internal (fixed) mic
    is the default, the jack mic is used while its pin senses presence. First input stream descriptor,
    48 kHz s16le stereo, 64 KiB ring, BDL in the playback BDL page at +2 KiB, stream tag 2. Pin: IN_EN
    (+VREF80 on a jack mic), boost amp one step above 0 dB, ADC input amp 0 dB unmuted.
  - Jack: presence polled from the audio server's 100 Hz pump, throttled to ~3 Hz (interrupts stay off,
    as for playback). Headphone plugged ⇒ speaker pins' OUT_EN cleared; unplugged ⇒ restored. A
    headset-mic change restarts an active capture on the other pin.
  - Display codec: every other codec in STATESTS is walked read-only; Intel vendor bit "enable all
    pins/converters" (nid 0x08 verb 0xF81/0x781) set first; pin sense, ELD-valid, ELD size/name/SAD count.
  - SND_GET_INFO gains `capture:`, `jack:` and per-pin raw `pin … sense …` lines.
- `servers/pipewire/src/lib.rs`: capture ABI = `read()` on `/dev/pipewire` opened O_RDONLY (one reading
  open at a time, EBUSY otherwise; no effect on the single writer). A read blocks until ≥10 ms of audio
  or `count` is filled (≤500 ms). The stream starts on first read and stops at that open's last close.
  Non-HDA backends return ENODEV.
- `userland/sndtest`: `sndtest record [sec]` (RMS/peak/dBFS per channel, PASS = ≥90 % of real-time
  frames and not digital silence) and `sndtest jack`.

## Evidence
- QEMU aarch64 `LEANDROS_SND=hda-micro`: path `0x04<-0x05`, 96000 frames in 2.02 s (47.5 k fps), zeros
  (QEMU's audiodev gives silence), stream stops on close; `sndtest tone` still fine afterwards.
- Real laptop (trial `#1 lapaudio2`, booted good back afterwards):
  - `sndtest record 3`: 144000 frames in 3.00 s (47.9 k fps), run 1 RMS -21.4 dBFS (peak 32768 — a
    start-up transient), run 2 RMS -61.8 dBFS peak 125 (room noise) → PASS. Internal mic path
    `0x13<-0x1a`, headset mic `0x13<-0x19`.
  - Pin sense: hp 0x1d sense 0x00000000 (empty), jack mic 0x19 0x00000000; speaker auto-mute armed.
  - Display codec 2: 8086:280b (Intel HDMI), pins 0x05/0x06/0x07 DP/HDMI (pincap 09000094), converters
    2/3/4, no monitor ⇒ present=0, eld_valid=0.

## Open
- Plug test of headphone/headset (needs a person at the laptop): expect `[HDA] jack: headphone plugged
  -> speakers muted` in dmesg and `jack: headphone=Some(true)`.
- First ~tens of ms of a capture carry a start-up transient (clips once); drop the first period if it bothers a consumer.
- poll(2) on /dev/pipewire still reports never-readable (static POLLOUT): a reader that polls before
  read will park. PipeWire source node (leandros-snd-source) for Firefox/WebRTC is not written.
- HDMI/DP playback: needs the i915 side (audio power well already on by BIOS PW2, ELD write +
  AUD_PIN_ELD_CP_VLD/audio enable, AUD_CONFIG N/M for DP, CDCLK) plus codec stream→converter→pin
  mapping; only provable with a monitor. Natural home: lane lapdisp follow-up.
