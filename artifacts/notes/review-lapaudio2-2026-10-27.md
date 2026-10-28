# Review: lane/lapaudio2 (merge 1b87d219), 2026-10-27

Scope: `git diff 1b87d219^1 1b87d219` (drivers/src/hda.rs, drivers/src/hda_capture.rs, servers/pipewire/src/lib.rs,
userland/sndtest) and the lane note. Review only, no code changed.

Verdict: playback on QEMU and the aarch64 virtio-snd path are not regressed, and the capture DMA is correctly kept
apart from playback. There are no High findings. Four Medium findings should be fixed before the poll/source-node
follow-up, because that work touches the same code (the tick pump and capture_read).

## Checked and OK
- Separate descriptors: capture uses SD 0 (the first input), playback uses SD `iss` (the first output) or a
  bidirectional one. Tag 2 vs tag 1 (and input and output tags are separate namespaces anyway). The SSYNC and SD_STS
  RW1C writes each touch only their own stream's bits. INTCTL stays 0, so INTSTS is never involved.
- Shared BDL page: playback uses 16x16 = 256 B at +0, capture is at +2 KiB, and reconfigure/recover never rewrite either.
- Codec verbs are all issued under the STATE lock (`&mut Hda`), so jack polling cannot interleave verbs with
  stream programming. `tick_pump` uses try_lock and skips when `!initialized`, so there is no polling after `shutdown()`.
- Reader closing mid-DMA: `capture_stop` clears RUN and waits, and the ring is never freed, so there is no
  use-after-free. `Hda::shutdown` stops every SD and unbinds every ADC, so kexec/reboot quiesce covers capture.
- EBUSY leak: READER is released on VFS_CLOSE by the same mechanism as WRITER. A dead reader frees it when its
  last fd goes away.
- virtio-snd (aarch64): `capture_read` returns ENODEV. Playback and pump paths are unchanged.
- The ELD parse (MNL = b[4]&0x1F, SAD count = b[5]>>4, name at byte 20, ELDV = sense bit 30) and the Intel
  vendor verb (nid 0x08, 0xF81/0x781, bit 0) match the spec and Linux for HSW through KBL/CFL.

## Findings

### M1: unconnected "speaker" pins get OUT_EN set at boot
- `drivers/src/hda_capture.rs:166-167` (find_jack_pins), `:349-357` (poll_jacks)
- Scenario: Realtek's default config for an unused pin, `0x411111f0`, has device = 1 (Speaker) and connectivity = 01
  (none). `spk_pins` does not filter on connectivity. At probe, `poll_jacks(true)` sees `hp_present` go from None to
  Some(false), counts that as a change with `on = true`, and ORs OUT_EN into every such pin. On every unplug it does
  the same again. As a result the driver drives pins that the playback path deliberately left off, which costs power
  and can cause noise or contention if the "unused" pad is wired to something.
- Fix: skip pins with `cfg >> 30 == 1` in `spk_pins`. Keep only speaker pins that `program_codec_paths` enabled,
  and save their original PIN_CTL at probe. Mute by clearing OUT_EN, and unmute by restoring the saved value, never by
  OR-ing. Do not apply auto-mute on the forced initial poll when `hp == Some(false)` (only record the state).

### M2: read() returns 0 (EOF) when no data arrives
- `servers/pipewire/src/lib.rs` `capture_read` (Ok(0) branch, 500 ms timeout), `hda_capture.rs:312` (`& !3`)
- Scenario: a stalled capture DMA (see L3), an LPIB that does not move, or `count < 4` makes read return 0 after
  500 ms. POSIX readers (PipeWire, arecord-style loops, `sndtest` before its deadline) treat 0 as EOF and stop. O_NONBLOCK
  is ignored, so the reader always sleeps up to 500 ms.
- Fix: never return 0 for `count >= 4`. When the caller set O_NONBLOCK, return -EAGAIN (-11) right away if nothing is
  available. When it is blocking, keep sleeping until data arrives, a pending signal returns -EINTR, or a long stall
  (2 s or more) returns -EIO after one `capture_start()` retry. For `count < 4`, return -EINVAL. Find out how the VFS
  forwards the open flags (VFS_OPEN arg1 already carries them, so record them per open_id when the reader is claimed).

### M3: codec verbs and a full capture restart run inside the BSP timer IRQ
- `drivers/src/hda.rs:1229` (`top_up_silence` -> `poll_jacks`), `hda_capture.rs:340-369`; the caller is
  `tick_pump` (sched tick hook)
- Scenario: before this lane the pump did MMIO reads and a memset. Now it does 2 to 4 CORB round trips every 300 ms,
  plus `udelay(100)` per TRIG_REQ pin. A codec that does not answer costs 20 ms per verb inside the IRQ, and the first
  timeout permanently switches the whole link to the immediate interface (see M4), all from IRQ context. A headset-mic
  change runs `capture_start()` inside the IRQ: SRST waits of up to 3 ms, about 15 verbs, and an hlog.
- Fix: keep only MMIO in the tick (`update()` + `capture_update()`, see L1). Move `poll_jacks` to thread context:
  either a small audio kthread that wakes every 300 ms and takes `STATE.lock()`, or call it from `capture_read` /
  VFS_POLL / write paths when it is due. A kthread is cleaner, and the poll follow-up below needs one anyway.

### M4: the display-codec probe can push the main codec onto the immediate interface for good
- `hda_capture.rs:397-411`, `hda.rs:710-720` (`command()` fallback is global and one-way)
- Scenario: STATESTS showed the codec at reset, but its power well (i915 PW2/audio domain) has since been gated, or
  the codec simply does not answer. The first CORB timeout stops CORB/RIRB and sets `immediate = true` for all later
  commands, analog playback included. On SKL+ the immediate interface is optional or unreliable (Linux treats
  single_cmd as a last resort). Before this lane, codecs after the analog one were never touched. The probe also
  leaves the display AFG in D0, and `shutdown()` only puts the main codec into D3.
- Fix: probe the display codec with a non-escalating verb (`send_corb` directly, giving up on that codec after the
  first timeout without falling back), or make the fallback per-call and only for `cad == main`. Record the display
  cad/afg and put it into D3 in `shutdown()`.

### L1: capture position is sampled only about 3.3 times a second while no read is in flight
- `hda_capture.rs:344` (`capture_update` only inside the throttled `poll_jacks`), `:288-298`
- Scenario: the ring holds 341 ms and updates arrive every 300 ms or more, plus skipped ticks (try_lock). If the gap
  exceeds one ring, the LPIB delta wraps, `cap_total` silently undercounts by 64 KiB, and the reader gets stale audio
  with no overrun counted.
- Fix: call `capture_update()` on every tick (it is one MMIO read), independent of the jack throttle.

### L2: a close racing an in-flight read restarts the stream with no owner
- `lib.rs` VFS_CLOSE vs `capture_read` loop; `hda_capture.rs:304` (auto-start in `capture_read`)
- Scenario: thread A is blocked in read() when the open's last fd is closed and VFS_CLOSE is processed (if the VFS
  allows that). `capture_close` stops the stream, then A's next loop iteration calls `capture_start()`. DMA, the ADC and
  mic bias then keep running with READER == 0 until some later reader closes.
- Fix: start the stream only from the claim path. Make `capture_read` never auto-start, or pass the owner `h` and
  re-check `READER == h` under the STATE lock before starting.

### L3: capture DMA errors are invisible and never recovered
- `hda_capture.rs:291-292`: FIFOE/DESE are cleared without a counter or a log, and LPIB stops moving. Combined with M2,
  the reader then sees EOF.
- Fix: count `cap_dma_errors`, log the first few, and restart the capture (as `recover_stream` does for playback)
  when LPIB has not moved for 100 ms or more while active.

### L4: LPIB can run ahead of memory, and nothing orders the copy after it
- `hda_capture.rs:293`, `:311-317`
- Intel controllers report a capture LPIB that can be up to a FIFO's worth ahead of the data actually landed in RAM
  (Linux's `bdl_pos_adj`). Copying right up to LPIB can return a few stale bytes from the previous lap. There is also no
  fence between the volatile LPIB read and the plain memcpy.
- Fix: subtract the input stream's `SD_FIFOS` (rounded up to 4) from `avail`, and add `fence(Ordering::Acquire)`
  after the LPIB read.

### L5: mic bias stays on after close
- `hda_capture.rs:243-251`: `capture_stop` unbinds the ADC but leaves the pin at IN_EN plus VREF80.
- Fix: write `PIN_CTL = 0` on the `cur_path` pin in `capture_stop`.

### Info
- GLK/ICL+ Intel HDMI codecs use vendor nid 0x0b, not 0x08. That is fine for KBL, but the probe should key the nid on
  the device id.
- On Realtek combo jacks the headset mic often needs vendor COEF headset-mode setup (CTIA/OMTP detect), and its pin may
  not report presence on its own. The headset path is still untested (nobody has plugged anything in).
- `userland/vfstag/vfstag.c:442` (`pipewire_poll`) asserts that /dev/pipewire is never readable. It must change with
  the poll work below.

## Design for the follow-ups (ready for fast-worker)

### A. Poll readiness (kernel, servers/pipewire)
1. Add an audio kthread at 100 Hz, or reuse the tick for MMIO only. In the tick, run `capture_update()` (L1). In the
   kthread, run `poll_jacks` (M3).
2. Claim and start in one place: an O_RDONLY open that issues VFS_POLL with POLLIN, or VFS_READ, claims READER and
   starts the stream (thread context). A poller that is not the holder gets POLLERR, or simply never POLLIN.
   Starting on poll matters: today the stream starts only on the first read, so a poll-before-read client would wait
   forever.
3. VFS_POLL for the holder: POLLIN when `cap_total - cap_read >= 1920` (one 10 ms period). The writer keeps POLLOUT
   unchanged. Use a dynamic poll tag for the reader instead of `static_poll_tag()`, and have the pump (tick or
   kthread) wake it when avail crosses one period. Follow whichever existing tick-driven char device already does
   `vfs_server` wakes (grep for the wake API next to `static_poll_tag`).
4. With poll in place, O_NONBLOCK reads return -EAGAIN (M2). Update vfstag `pipewire_poll` to expect POLLIN from
   the reader and no POLLIN from a non-reader.
5. Optionally discard the first 960 frames after each `capture_start` in the kernel to hide the startup transient.

### B. PipeWire source node (ports/pipewire)
- Write `leandros-snd-source.c`, mirroring `leandros-snd-sink.c`: an SPA Audio/Source node, S16LE, 48 kHz, 2 ch,
  quantum 480. Open `/dev/pipewire` O_RDONLY|O_NONBLOCK and add the fd to the data loop (POLLIN). On readiness, read
  one quantum and push it to the output port buffer. On EAGAIN, wait. On EBUSY at the first read, retry once a second
  and report the node as unavailable meanwhile.
- Register it in `ports/pipewire/data/50-leandros.conf` as `node.name = leandros-snd-source`,
  `media.class = Audio/Source`. Test with `pw-record` and Firefox getUserMedia on the laptop.

### C. HDMI/DP playback (later, needs a monitor; lane lapdisp follow-up)
- i915 side (KBL, per Linux `intel_audio.c` hsw_audio_codec_enable, read as documentation only): audio power well on
  (PW2) and CDCLK at or above the audio minimum; for each active transcoder, clear ELD_VALID and AUDIO_OUTPUT_ENABLE in
  `HSW_AUD_PIN_ELD_CP_VLD`, reset the address in `HSW_AUD_DIP_ELD_CTRL`, write the ELD dwords to `HSW_AUD_EDID_DATA`,
  then set ELD_VALID and AUDIO_OUTPUT_ENABLE. `HSW_AUD_CFG`: for HDMI, the pixel-clock index for the mode; for DP,
  `AUD_CONFIG_N_VALUE_INDEX`, with programmed N/M (`HSW_AUD_M_CTS_ENABLE`) only for non-standard link rates. Build the
  ELD from the EDID that i915 already reads. Redo all of this on modeset and on hotplug.
- HDA side: a second output SD (`iss + 1`) with tag 3 on the display codec cad. Pick a free converter, set its channel
  count (0x72D) and format, bind the stream (0x706), and enable the digital converter (0x70D DIGEN). On the pin, select
  the converter (0x701), set PIN_CTL OUT_EN, and write the audio infoframe through the DIP verbs (0x730/0x731/0x776).
  Have the 300 ms jack kthread watch ELDV and re-read the ELD.
- Routing: expose it as a second sink. Prefer a separate node (`/dev/pipewire-hdmi`, its own WRITER) over switching
  inside the single writer, so the analog path is never reprogrammed by a monitor hotplug.
