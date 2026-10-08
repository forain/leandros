# Audio

Sound leaves a LeandrOS guest through one virtio-sound playback stream, driven by a kernel driver and fed by a small in-kernel audio server that owns a spool and paces producers to the audio clock. Desktop sessions run real PipeWire, WirePlumber and pipewire-pulse on top of that device, so COSMIC's volume controls, Firefox and `pw-play` work unmodified; console programs (`aplay`, MAME, DOOM outside a session) talk to the kernel server directly.

## Overview

```
 Firefox (libpulse) ─► pipewire-pulse ─┐
 cosmic-settings-daemon (libpipewire) ─┼─► PipeWire 1.2.7 graph ─► leandros-snd-sink ─┐
 pw-play / pw-cat / DOOM ──────────────┘   (WirePlumber policy)     (graph driver)     │ write(/dev/pipewire)
                                                                                        ▼
 aplay / MAME / DOOM on the console ── legacy IPC (auxv port) ──► kernel audio server (servers/pipewire)
                                                                   64 KiB spool, tick pump, stall recovery
                                                                                        ▼
                                                                  virtio-snd driver (drivers/src/snd.rs)
                                                                                        ▼
                                                                  QEMU virtio-sound-pci ─► host audio backend
```

| Layer | Code |
|---|---|
| virtio-snd driver | [`drivers/src/snd.rs`](../drivers/src/snd.rs) |
| Kernel audio server | [`servers/pipewire/src/lib.rs`](../servers/pipewire/src/lib.rs) — named for the socket path it binds; it is not PipeWire |
| PipeWire port and sink | [`ports/pipewire/`](../ports/pipewire) — [`leandros-snd-sink.c`](../ports/pipewire/leandros-snd-sink.c), configuration in [`data/`](../ports/pipewire/data) |
| Console player | [`userland/aplay`](../userland/aplay/src/main.rs) |

## The virtio-snd driver

The driver implements the `virtio-snd` specification over modern virtio-PCI:

- Control, event, TX and RX virtqueues are set up (sized by negotiation, at most 256); playback uses TX. There is no capture.
- PCM stream lifecycle `SET_PARAMS → PREPARE → START → STOP → RELEASE`, with status words checked against `VIRTIO_SND_S_OK`. Reconfiguration tears down unconditionally — `STOP`/`RELEASE` on a never-started stream is a harmless `BAD_MSG`.
- S16LE at 44.1 kHz and 48 kHz, stereo (mono accepted from producers).
- Robust negotiation: control commands time out instead of hanging on a host that does not answer every command.
- `send_pcm_data` is non-blocking: it enqueues 512-byte TX buffers (~2.9 ms at 44.1 kHz) and returns the number of bytes accepted, so callers can apply back-pressure without blocking in the driver.
- Ring publication uses real memory barriers and volatile `avail.idx` stores; a compiler-only fence was wrong under HVF SMP.
- The device's INTx is disabled; the driver is polled from the audio server's tick pump.

**Queue size is negotiated, not assumed.** QEMU 11.1 (a fix for CVE-2026-50626) marks the device broken when a driver sets a queue larger than its default of 64; the original driver wrote 256 and, on the upgraded host, got no audio at all — every control command timed out. With 64 descriptors and three per TX buffer, 21 buffers (~61 ms) can be in flight.

**Only whole frames are sent** (`d846d66`). QEMU's audio core returns 0 for a write shorter than one frame, and virtio-snd treats 0 as "backend full" and never returns the buffer, so a TX buffer ending in a 1–3 byte remainder sat at the head of the queue forever: 21 buffers held, ring full, no kicks, and 250 ms later the stall detector restarted the stream. It showed up as "MAME is a bit slow and audio clips" — 27–30 recoveries per minute mid-music. `send_pcm_data` now rounds each chunk down to whole frames and returns 0 for a sub-frame remainder, which the spool merges with the next push. Larger TX buffers made it *worse* (more unaligned tails), not better. The device-side behaviour — neither completing nor erroring the message — is a QEMU bug worth reporting upstream.

## The kernel audio server

The server owns the device on behalf of everyone:

- **`/dev/pipewire`** (character device 116:16, `root:audio 0660`) — `write()` is raw interleaved S16 PCM with back-pressure; ioctl `0x101` is `SET_PARAMS {u32 rate, u8 channels}` and `0x102` is `GET_DELAY`, the bytes queued ahead of the DAC (spool plus device ring). Every ioctl on the server's device port is routed to it; they once fell through to the TTY server and answered `ENOTTY` (`1feffbc`).
- **A legacy IPC protocol** (message tags `0x100` PCM, `0x200` set-params) for `no_std` console programs. The server's port is stamped into every task's auxiliary vector as `AT_LEANDROS_AUDIO_PORT` (tag 258), so `get_audio_port()` finds it in O(1) with no name service — the [auxv service-discovery](kernel/ipc.md) design. The server also force-binds the Unix path `/run/pipewire/pipewire-0`. IPC clients must pass the same permission check as the node: root or the `audio` group.
- **Single writer.** One spool feeds one stream, so two producers would interleave samples into garbage. Whoever holds the device — an open of `/dev/pipewire` for writing, or an IPC client — keeps it until its last fd closes or it exits; everyone else gets `EBUSY`. Mixing is PipeWire's job, exactly as `aplay -D hw:0` is refused on Linux while PipeWire holds the card.

### Asynchronous pipeline with back-pressure

A 64 KiB **spool** sits between producers and the hardware ring. Clients never block on a full device ring; the server drains the spool into it in non-blocking chunks, decoupling client timing from the device's cadence. Three mechanisms keep it alive on QEMU:

1. **Back-pressure** (`push_spool_blocking`). When the spool is full the push drains to the ring and waits for QEMU to consume, bounded so a dead stream cannot wedge the caller. Before this, everything beyond ring plus spool was silently dropped: MAME ran 15 emulated seconds in under 8 wall seconds, and the long-standing "aplay proof point" had only ever played 0.9 s of a 5.89 s test file. Back-pressure also paces producers exactly to the audio clock — MAME runs at 100.0% speed.
2. **A 100 Hz tick pump** (`sched::register_tick_hook`, `try_lock` only) drains the spool and **pads silence** whenever fewer than 16 KiB (~93 ms) are queued. QEMU 11.x's split audio backend permanently loses its refill wakeup the first time it polls an empty stream queue — every control command still returns OK while buffers are accepted and never completed — so the queue must never run dry, including between songs and after a program exits (which also stopped the tail of every playback being lost).
3. **A wall-clock stall detector**: if the TX used index has not moved for 250 ms while data waits, `recover_stream()` performs an unconditional `STOP/RELEASE/SET_PARAMS/PREPARE/START`. Spin-count thresholds false-triggered on QEMU's bursty ~12.5 ms tick gaps, so the detector uses the monotonic clock. One `[SND] TX stalled — recovering stream` line per program start is normal; each recovery discards ~0.25 s.

**Latency.** Spool plus in-flight bytes is the steady-state latency on the console path: about 610 ms after the spool was cut from 128 KiB to 64 KiB (from ~971 ms). It cannot go lower on QEMU 11.0–11.1: after any restart QEMU's first poll gulps a whole buffer's worth and timer slips multiply per-tick demand, so the spool must refill instantly; at 32 KiB the stream collapses. Small configurations are bimodal — one passed two runs and then failed three — so a single clean run proves nothing. Lazy `START`, resubmitting unplayed buffers on recovery, smaller in-flight caps and backend rate mismatches were all tried and failed.

## PipeWire sessions

A graphical session runs Alpine 3.21's **PipeWire 1.2.7, WirePlumber 0.5.7 and pipewire-pulse** (plus `pw-cli`, `pw-cat`, `pw-play`, `wpctl`), staged by [`ports/pipewire/build.sh`](../ports/pipewire/build.sh) and started once per runtime directory by the session launcher; PipeWire's `context.exec` then starts WirePlumber, the sink and pipewire-pulse. There is **no ALSA ABI** (no `/dev/snd`, no `hw_params`, no mmap status page), so the only output is a PipeWire client:

**`leandros-snd-sink`** is an `Audio/Sink` node that drives the graph, timed like spa-alsa's timer-scheduled mode: a timer per quantum, `GET_DELAY` to read the device's queue depth, and a DLL steering the rate to keep 4096 frames queued. Two simpler designs were rejected: blocking writes would keep the 64 KiB spool full (adding a second of latency) and spin a CPU in the back-pressure loop, and a writer paced by PipeWire's dummy driver would let the tick pump's silence interleave with real samples. The graph runs at 48 kHz with a 1024-frame quantum. WirePlumber's profile disables the ALSA, Bluetooth, video, logind and reserve-device monitors that cannot work here; the standard policy (default node, linking, volume restore) treats the sink like any virtual sink.

**What COSMIC needs.** COSMIC's audio applet, OSD and Settings → Sound page are varlink clients of `cosmic-settings-daemon`; only the daemon speaks PipeWire (via `libpipewire-0.3`), reading the default sink from WirePlumber's metadata and setting volume through node properties. Before real PipeWire shipped, the daemon was linked against an inert stub `libpipewire` exporting the 63 `pw_*` functions it imports, every constructor returning NULL — safe because the daemon's PipeWire thread treats creation failure as "retry later" and never dereferences anything. The real 1.2.7 library has the same ABI, so the stub was swapped out with no rebuild of the daemon or the portal.

**Firefox** reaches sound through **pipewire-pulse**. Its cubeb backend tries PulseAudio first, by `dlopen`ing `libpulse.so.0` — which the dependency walk never saw, so it was staged explicitly. Two kernel bugs then hid behind each other: musl's `PTHREAD_PRIO_INHERIT` mutex probe needed **PI futexes** (`FUTEX_LOCK_PI` was `ENOSYS`, and libpulse asserts), and libpulse's mainloop wake-up `send()`s on a pipe and falls back to `write()` only on **`ENOTSOCK`**, which the kernel answered as `EBADF`. Both are implemented now; see [Threads and futex](kernel/threads-and-futex.md) and [Networking](servers/networking.md).

**Start-up ordering.** The launcher starts PipeWire as a background job and then `exec`s the session. brush ran `&` lists as an in-process task, so an `exec` before the task spawned anything lost PipeWire entirely (empty log, no pidfile) — on aarch64 images built from an unpatched brush only. The brush patch that waits at `&` until the first process is spawned (bash's ordering) fixes it, and `build-all.sh` now refuses to build against a brush checkout that is not the pinned, patched one (`e5e17d3`).

**DOOM** in a session plays through PipeWire by piping to `pw-cat`, since the device is held by the sink and a direct write gets `EBUSY`.

## `aplay`

`aplay` ([`userland/aplay`](../userland/aplay/src/main.rs)) is a `no_std` console player that streams to the kernel server's IPC port, found through the auxiliary vector:

- `.wav` files (16-bit PCM),
- `.mid` files through a built-in software synthesiser,
- `test` to generate a 5-second reference tone.

While a PipeWire session holds the device, `aplay` gets `EBUSY` (by design; use `pw-play` there).

## Measured results

| Check | Result |
|---|---|
| Console path, MAME attract music, aarch64/HVF | 0 recoveries in 4 runs after the whole-frame fix; 100.0% emulation speed |
| `aplay` test file, x86_64 | full 5.900 s (was 5.437 s before the whole-frame fix) |
| PipeWire tone at volume 1.00, both arches | 10.0–10.1 s, RMS 5771–5788 against an ideal 5792, no holes |
| `wpctl set-volume 0.30` | RMS 155.9 = 0.027 × full = 0.30³ (PipeWire's cubic volume) |
| COSMIC Settings slider to 80 | `wpctl` 0.80; RMS ratio 0.512 = 0.8³ |
| Sink rate correction | 1.0011–1.0013, delay 2304–4480 frames around the 4096 target, 0 underruns |
| Firefox, Opus and VP9+Opus test tones, aarch64/HVF | 12.1 s, RMS 1022–1024 (unity gain), correct frequency |

## Design decisions

- **Keep the device single-writer and put mixing in PipeWire**, as Linux does with an ALSA hw device, rather than mixing in the kernel.
- **Pace producers by back-pressure** rather than trusting each program's own throttle.
- **Never let QEMU see an empty queue**, because its failure mode is permanent and silent.
- **Drive the PipeWire graph from the device's measured delay** rather than blocking writes or a free-running timer.

## How it is tested

Audio is verified at the destination: QEMU's `wav` backend captures the guest's output on the host (`LEANDROS_AUDIO_WAV=<file>`), and analysis looks at per-second RMS, frequency and zero-runs inside the audible region — not file size. Lessons that shaped this:

- An early "MAME audio" capture was leftover test-tone data from the spool; per-second RMS, not presence, is the check.
- A single-buffer liveness probe passes on a dead stream: QEMU consumes exactly one buffer per `START` before the voice dies.
- A zero-run metric over the wav file is blind to stalls, because QEMU writes nothing while a stream is stalled and the file simply time-compresses; recoveries are counted from the serial log instead.
- `min_level = 0` on a stall line is trivial (the ring starts empty after each recovery) and is not evidence of an underrun.
- QEMU's `-trace enable=a,enable=b` keeps only the last pattern; use `-trace events=<file>`.

## Limitations and open work

- Playback only, one stream; no capture, no ALSA ABI.
- The first playback of a session on x86_64 can show a ~40 ms gap about a second in — most likely a client xrun, since there is no real-time scheduling class and RTKit is absent. Steady playback is clean.
- PI futexes have no priority boosting or ownership hand-off; robust lists are not implemented.
- DOOM's audio clips heavily on x86_64; this predates the PipeWire path and is in DOOM's own mixing.
- Firefox audio on x86_64 has been verified only under TCG, where throughput gaps are expected.

## See also

- [Input](input.md) and [Graphics](graphics/README.md) — the other device stacks.
- [Desktop session](desktop/session.md) — where PipeWire is started, and the ScreenCast portal that also uses it.
- [Ports](userland/ports.md) — PipeWire, Firefox, MAME and DOOM ports.
- [Running](getting-started/running.md) — host audio backend selection (`coreaudio` on macOS; on Linux `pa` only when a live PulseAudio socket exists, else QEMU's `pipewire`, else `alsa`, else `none` — QEMU aborts at start-up if its backend cannot open, so a headless host must never get a guess).
