# Lane ffaudio (2026-10-03): Firefox plays sound

Bug: "I don't get any sound output from Firefox playing videos on Youtube."
Branch `lane/ffaudio`, base main `f994928`. Firefox 136.0.4 (Alpine 3.21, musl).

## Root cause: three layers, each hiding the next

1. **No PulseAudio anywhere (port).** cubeb runs in the parent process (audioipc
   server) and tries PulseAudio first (`dlopen("libpulse.so.0")`), then ALSA.
   Neither `libpulse` nor a pulse server was staged: libpulse is dlopen()ed, so
   the DT_NEEDED closure walk never saw it (same class as lane/ffgmp's FFmpeg),
   and ports/pipewire shipped no `pipewire-pulse`. ALSA (libasound is staged)
   cannot work: no `/dev/snd`, no ALSA ABI. So cubeb had no backend: silence.
2. **FUTEX_LOCK_PI was ENOSYS (kernel).** With libpulse staged, the audioipc
   server aborted the parent's audio thread at once:
   `Assertion 'r == 0 || r == 95' failed at ../src/pulsecore/mutex-posix.c:57,
   function pa_mutex_new()` (EXIT=139). libpulse creates PTHREAD_PRIO_INHERIT
   mutexes; musl's `pthread_mutexattr_setprotocol(PRIO_INHERIT)` probes
   `FUTEX_LOCK_PI` on a zero word and returns the error (ENOSYS) instead of
   ENOTSUP; libpulse asserts.
3. **send() on a pipe answered EBADF instead of ENOTSOCK (kernel).** Next run:
   no crash, a cubeb stream, still silence, and
   `pa_write() failed while trying to wake up the mainloop: Bad file descriptor`.
   libpulse's `pa_write` tries `send(fd, MSG_NOSIGNAL)` and falls back to
   `write()` only on ENOTSOCK. Socket syscalls went straight to the net
   server, which only knows its socket fd range and said EBADF for a pipe, so
   the threaded mainloop could never wake itself and the stream never started.

## Fix

- `ports/firefox/dlopen-in-alpine.sh` (was ffmpeg-in-alpine.sh): stages the
  dlopen()ed libraries — system FFmpeg and now **libpulse 17.0** (`libpulse.so.0`,
  `libpulsecommon-17.0.so` from /usr/lib/pulseaudio, +libasyncns, libsndfile,
  FLAC, mpg123: 6 new sonames) — and writes `/etc/pulse/client.conf`
  (`autospawn = no`). `build.sh <arch> --dlopen-only` (old `--ffmpeg-only`
  still accepted) adds them to an existing out/<arch> from a host-arch container.
- `ports/pipewire`: stages Alpine's **pipewire-pulse 1.2.7**
  (`libpipewire-module-protocol-pulse.so`, `pipewire-pulse.conf`, the
  `pipewire-pulse` -> `pipewire` argv[0] link; closure adds libavahi-client/
  common). `50-leandros.conf` starts it from PipeWire's `context.exec` next to
  WirePlumber and leandros-snd-sink; socket `$XDG_RUNTIME_DIR/pulse/native`.
  The session launcher also removes a stale `pulse/native`/`pulse/pid`.
  Firefox finds the socket through XDG_RUNTIME_DIR (session env); no prefs,
  no Firefox/COSMIC source changes.
- `kernel`: **PI futexes** — FUTEX_LOCK_PI / UNLOCK_PI / TRYLOCK_PI / LOCK_PI2
  on Linux's lock word (owner TID | OWNER_DIED | WAITERS), built on the
  existing futex wait queue: LOCK_PI takes a free word (keeping WAITERS set
  while others are queued) or sets FUTEX_WAITERS and parks expecting exactly
  that word; UNLOCK_PI releases to 0 and wakes one locker; EDEADLK, ESRCH,
  EPERM, absolute timeouts (REALTIME for LOCK_PI, MONOTONIC for LOCK_PI2) as on
  Linux. FUTEX_WAITERS is set before any timeout verdict so a giving-up waiter
  never strands the others. Not implemented: priority boosting, ownership
  handoff, robust lists (OWNER_DIED only preserved).
- `kernel`: **ENOTSOCK** — socket calls (send/recv/sendmsg/recvmsg/bind/
  connect/listen/accept/shutdown/get/setsockopt/getsockname/getpeername) on an
  open VFS-range fd (after socket-alias translation) answer ENOTSOCK, on a
  closed one EBADF (`not_a_socket` in dispatch_inner).
- Tests: pthreadtest `futex_pi_basic`, `futex_pi_contended` (4 threads x 3000,
  musl's user-space cas protocol + kernel slow path), `futex_pi_timeout`;
  scmtest `send_on_pipe_enotsock`. All fail on the old kernel (ENOSYS / EBADF).
- `ffsession.py --post CMD`: a serial command after the observation (wpctl).

## Evidence (host wav via LEANDROS_AUDIO_WAV)

Test page `lane-ffaudio-files/at.html`, served from the Mac
(`rangeserver.py`, 192.168.105.1 over vmnet): a VP9+Opus `<video>` with a
660 Hz tone, then an Opus-only `<audio>` with an 880 Hz tone, 12 s each. Source
level: lavfi sine (1/8) x0.5, mono upmixed -3 dB => RMS ~1024 per channel, so
~1020 measured means unity gain end to end.

| | aarch64 HVF (Mac) | x86_64 TCG (Mac) |
|---|---|---|
| video 660 Hz (VP9+Opus) | 12.1 s, RMS 1024, freq 660.0, 0 gaps | (TCG: video + audio overlapped, see below) |
| audio 880 Hz (Opus) | 12.1 s, RMS 1022, freq 880.0, 1 x 10 ms gap | 12.2 s, RMS 1018, freq 880.0, 22 short gaps = 0.36 s total (media.cubeb_latency_playback_ms=500) |
| 880 Hz direct URL | 12.1 s, RMS 1022, 0 gaps | default 100 ms latency: first ~5 s clean, then dropouts (TCG starvation) |
| wpctl | Firefox stream -> Virtio Sound, vol 1.00; clients incl. pipewire-pulse | same |
| pa_write / pa_mutex errors, SEGV/panic | 0 / 0 | 0 / 0 |
| runtests regress (14) | PASS (pthreadtest + scmtest new cases PASS) | PASS |
| native PipeWire (audio-session-test) | tone1 RMS 5788, tone2 (0.30) RMS 155.9 — same as lane/pipewire | pw-play RMS 5688, 4 x 50 ms holes (TCG) |

x86_64 KVM was not available: both Linux machines were unreachable (No route
to host, Tailscale timed out) for the whole lane. On TCG even native pw-play
has holes, and other lanes' QEMUs were running on the Mac at the same time, so
the x86 dropouts are throughput, not the audio path; re-check on KVM.

YouTube itself could not be tested: Google answered the guest with the
"unusual traffic" reCAPTCHA page from this network.

## Open

- **x86_64 KVM** verification of the Firefox tone (desktop/laptop offline).
- **WebM ends in a demuxer error** (both arches, independent of audio): the
  last cluster of these ffmpeg-made WebMs fails in Firefox's demuxer —
  progressive playback decodes every packet to 12.001 s and then reports
  NS_ERROR_DOM_MEDIA_DEMUXER_ERR instead of EOS (`ended` never fires), and an
  MSE append demuxes clusters 1-2 and fails on cluster 3. Bytes reach the
  page intact (FNV per 16 KiB block matches the host), the file parses as
  well-formed EBML, and removing the Tags element changes nothing. Not
  investigated further; YouTube's MSE video plays for the user, so check
  whether real DASH WebM shows it before chasing (msea.html reproduces).
- No priority inheritance (no RT class); PI unlock does not hand ownership
  over (a woken waiter competes for the free word like any locker).
- pipewire-pulse warns about rtkit (no RT promotion), harmless.
- ENOTSOCK for epoll-range fds (>= 0x400) is still EBADF.
