# lane/vt2laptop (2026-10-25): VT2 games on the laptop (i915)

## VT2 measurement (trial kernel with /proc/vtstat switch_log, scripted desktop login, gv.sh-style run, 1 s samples)
- DOOM then MAME as root after `vttest chvt 2`: VT2 stayed active (`active 2`) for the whole run; the only switches in
  switch_log were `1>2:ioctl` (our vttest) at start and `2>1:ioctl` (our vttest, tgid != cosmic-comp/greetd) at the end.
  Nothing bounced back to VT1. cosmic-comp DID drop master on the switch (`drop_ok 0->1`, master_by_vt vt1 4->0), DOOM got
  VT2's grant implicitly (`implicit 1`, vt2=<doom pid>), MAME the same after DOOM was killed. Both played full screen on VT2
  (user-confirmed). The earlier "VT2 refused / bounce" reports did not reproduce on this kernel (cb1dc55f + vt2master).
- Only remaining defect: no sound.

## Silent DOOM on VT2: FIXED (365ffc0c on lane/vt2laptop)
- doom.log of the silent run: `sound: the audio device is busy (held by a PipeWire session ...) and this session's PipeWire is
  not reachable (XDG_RUNTIME_DIR/pipewire-0); no sound`. The desktop's PipeWire (leandros-snd-sink) owns the audio device;
  DOOM must stream through pw-cat, but a root shell on a text VT has no XDG_RUNTIME_DIR, so the socket
  `/run/user/1000/pipewire-0` was never found.
- Fix: scripts/vendor/doomgeneric/leandros_sdl.c `audio_open_pipewire` falls back to /run/user/1000 when XDG_RUNTIME_DIR is unset
  or has no pipewire-0 and passes XDG_RUNTIME_DIR to pw-cat. Verified on the laptop on VT2: `sound: PipeWire stream via pw-cat`,
  soundfont loaded; user heard perfect sound (first with the env set by hand, then with the patched binary and no env).
- OPEN: MAME (src/osd/leandros/leandros_sound.cpp) sends PCM straight to the kernel audio server port (AT_LEANDROS_AUDIO_PORT)
  and ignores replies; while the desktop's PipeWire sink holds the device that is silent. Needs the same pw-cat route (spawn
  `pw-cat --playback --rate 44100 --channels 2 --format s16 -` with XDG_RUNTIME_DIR=/run/user/1000 and write the PCM to its
  stdin, fall back to the IPC port when no socket). Not done in this lane (separate repo, rebuild of MAME).

## Boot oddities (logs artifacts/remote-logs/20261025-135807-*)
- (b) Trial booted again although default=good armed=0: BOTH trial boots' `espctl boot` printed `default=trial` (the config on
  the ESP still named the trial as default at the second boot), although the first one printed "one-shot trial: the next boot
  defaults to good". So the default reset written by the first trial boot did not survive (that boot's klog ends 71 KB in, right
  after iwlwifi INIT calibrations, and it was power-cycled). Not a Limine memory effect. Cause of the lost ESP write is unproven
  (candidate: usbms write not durable when the machine stalls/is cut, since espctl only fsyncs the node); espctl's step order
  itself syncs after each step.
- (a) First trial boot refused root key auth for 8+ min: no trace survives: persistent /var/log/sshd.log has no `ready` line for
  that boot (the next ready is the second trial boot), the host key te5Z... is the same one used since September, and the klog
  of that boot stops early. Rootfs/sshd state of that boot was not persisted; the machine answering on :22 with "a new host key"
  could not be tied to this rootfs. Treat as one-off stall (klog stop at iwlwifi bring-up) until it recurs; if it does, capture
  serial/screen at that moment.
