---
name: run-leandros
description: Run, start, boot, launch, test, screenshot, or interact with LeandrOS in QEMU. Use when asked to run the OS, verify a kernel change, test userspace, or take a screenshot of LeandrOS.
---

LeandrOS is a bare-metal Rust microkernel that boots in QEMU. This skill drives it
headlessly via two Unix sockets: one for the serial console (PL011 UART, first `-serial`
port) and one for the QEMU monitor. The driver lives at
`.claude/skills/run-leandros/driver.py` and is the primary agent path — run it instead
of `run-qemu.sh`, which opens an interactive window.

## Prerequisites

```sh
# macOS (Homebrew QEMU already installed; all deps present in this repo)
which qemu-system-aarch64   # /opt/homebrew/bin/qemu-system-aarch64
which qemu-system-x86_64    # /opt/homebrew/bin/qemu-system-x86_64
python3 --version            # Python 3.x — stdlib only, no pip installs needed
```

Pre-built images are checked into the repo root and ready to use:
- `leandros-limine-aarch64.img`  — AArch64 UEFI Limine disk image
- `leandros-limine-x86_64.img`   — x86_64 UEFI Limine disk image

If you need to rebuild after a code change run `./scripts/build-all.sh` first.

## Run (agent path)

All commands run from the **repo root**:

```sh
cd /Users/forain/code/leandros

# 1. Boot (defaults to aarch64; takes ~5s on this machine). On an Apple
# Silicon host, aarch64's default "uefi" mode now boots with HVF acceleration
# automatically (fixed 2026-07-15). Pass "uefi-tcg" to force software
# emulation instead. x86_64 is always TCG (no cross-arch HVF).
python3 .claude/skills/run-leandros/driver.py start aarch64
python3 .claude/skills/run-leandros/driver.py start x86_64
python3 .claude/skills/run-leandros/driver.py start aarch64 uefi-tcg

# 1b. LOG IN — boot lands on a "login: " prompt (getty loop in init), not a
# shell. Seeded accounts: root/root (uid 0, /root) and leandro/leandro
# (uid 1000, /home/leandro); shell is /bin/brush for both. Shell exit
# respawns a fresh login prompt. Run root-expecting tests (vfstest's
# permission/chroot cases, etc.) as root — as leandro they fail with EPERM
# by design, and a failed non-root run can leave residue on the persistent
# image that makes a later root run fail too (regenerate images via
# scripts/mkfs-f2fs-populated.py if in doubt).
python3 .claude/skills/run-leandros/driver.py login root root

# 2. Send a shell command, get output. Optional third arg = read-timeout in
# seconds (default 8) — pass a larger value for long-running commands like mame.
python3 .claude/skills/run-leandros/driver.py cmd "help"
python3 .claude/skills/run-leandros/driver.py cmd "ls /bin"
python3 .claude/skills/run-leandros/driver.py cmd "mame captcomm -rompath / -str 30 -skip_gameinfo" 90

# 3. Screenshot (GPU framebuffer → PPM + auto-converts to PNG on macOS)
python3 .claude/skills/run-leandros/driver.py screenshot /tmp/screen.ppm

# 4. Check status
python3 .claude/skills/run-leandros/driver.py status

# 5. Full serial log
python3 .claude/skills/run-leandros/driver.py log

# 6. Stop
python3 .claude/skills/run-leandros/driver.py stop
```

Only one QEMU instance runs at a time. `start` refuses if one is already running.
`stop` sends `quit` to the QEMU monitor then SIGTERMs as fallback.
QEMU's stderr goes to `/tmp/leandros-qemu-stderr.log`.

### Audio capture (headless verification)

Guest audio is discarded by default (`-audiodev none`). To verify audio
headlessly, set `LEANDROS_AUDIO_WAV=/path/out.wav` on `start` — QEMU then
records everything the guest plays through virtio-sound into that wav
(backend-native 44.1 kHz stereo S16). The RIFF header is only finalized when
QEMU exits; while running, parse PCM from byte offset 44 and watch file size
(~176,400 B/s when a stream plays — silence still grows the file). Verified
workflow: `mame captcomm -rompath / -str 60 -skip_gameinfo` boots silent for
~24 emulated seconds, then plays attract music — check per-second RMS of the
captured PCM, not just file growth (a streaming-but-silent game grows the
file too).

`LEANDROS_QEMU_EXTRA` appends arbitrary QEMU args on `start` (shlex-split),
e.g. `-trace enable=virtio_snd_*,file=/tmp/t.log` for device tracing.

### Shell commands supported

The userspace shell (PID 1) accepts: `help`, `info`, `ls [path]`, `cd <path>`,
`pwd`, `test`, `clear`, `exit`, `<binary>` (executes from `/bin/`).
Binaries in `/bin/`: `init`, `shell`, `aplay`, `hello`, `doom`, `mame`.

### Screenshot notes

`screendump` captures the VirtIO GPU framebuffer even with `-display none`.
Output is a PPM file; the driver auto-converts to PNG via `sips` on macOS.
The framebuffer is 1280×800 (as negotiated with virtio-gpu-pci at boot).

## Run (human path)

```sh
./scripts/run-qemu.sh aarch64   # opens an interactive QEMU window (macOS Cocoa)
./scripts/run-qemu.sh x86_64
./scripts/run-qemu.sh aarch64 --tcg   # force software emulation instead of HVF
```

Same HVF-by-default-on-Apple-Silicon behavior as the driver (aarch64 UEFI mode
only); `--tcg` opts out, `--hvf` forces it on elsewhere (and fails to launch
there). The arch token (`aarch64`/`x86_64`) can go anywhere in the argument
list, not just first.

This is useless headless and blocks the terminal; use the agent path above.

### GPU path (COSMIC never renders in software)

`run-qemu.sh` picks the GPU device by default (`--gpu auto`): Venus
(`venus=on`, guest renders through zink) where the host QEMU/virglrenderer
supports it, virgl where only GL passthrough exists, nothing otherwise
(macOS Homebrew QEMU has no virglrenderer — it prints a warning). Override
with `--venus`, `--virgl`, `--no-gpu` or `LEANDROS_GPU=auto|venus|virgl|none`.

In the guest, `/bin/gpu-env` (sourced by `/etc/profile`, `greeter-real`,
`start-cosmic-leandros`) verifies a hardware `GL_RENDERER` with
`/bin/gpuprobe gl` before any compositor starts. No GPU renderer ⇒ the
graphical login is **not started**, init prints a `NO GPU RENDERER` banner on
serial, and `start-cosmic-leandros` exits 78. The serial login is unaffected,
so **`driver.py start` without `--venus` is the headless-test path** (plain
virtio-gpu, no greeter burning CPU). softpipe is explicit opt-in only:
`touch /etc/leandros/allow-software-render` (or `LEANDROS_RENDERER=software`).
The Mesa ship-set comes from `ports/mesa/build-gpu-stack.sh <arch>`
(→ `leandros-artifacts/m3-gl-stack/gpu-stage-<arch>`); mkfs warns if it is absent.

## Build

```sh
./scripts/build-all.sh              # both architectures (aarch64 + x86_64)
./scripts/build-all.sh --arch aarch64   # one arch only
```

Always build **release** targets — debug builds crash during early boot (see CLAUDE.md).
Build time: ~3–5 minutes clean, ~30s incremental.

## Gotchas

- **HMP `x` / `info registers` can hang QEMU's main loop for good under HVF
  (QEMU 11.1.1, in-kernel GICv3, macOS 26) — do not poll them as a liveness
  probe.** Both need `run_on_cpu` on a vCPU; roughly one call in a few hundred
  never completes (seen 4× on 2026-09-15, including on an *idle* guest with
  nothing else running). The monitor then stops answering, `quit`/SIGTERM are
  ignored (SIGKILL works), and — because every virtio device is processed by the
  main loop — the guest sees dead devices: `[GPU] control-queue TIMEOUT`, frozen
  audio/wav, `[PW] producer gap`, fork/exec blocked on virtio-blk. The probe
  manufactures the whole-guest wedge it was meant to diagnose. Liveness comes
  from the guest instead: `liveness-run.py` (below), the `[WDOG]`/`[TIMER]`
  kernel lines, and a userspace heartbeat. Serial output still works during a
  main-loop hang (PL011 TX runs on the vCPU thread), serial *input* does not.

- **`liveness-run.py <label> "<cmd>" [--wav] [--timeout S]`** runs one guest
  command with a userspace heartbeat, a persistent (nothing-dropped, timestamped)
  serial reader, optional wav-growth tracking, and — only at a stall — a
  host-side `sample` of the QEMU process (which vCPU threads spin in `hv_trap`
  vs sleep in the framework's WFI) before any monitor command. Output under
  `$LEANDROS_LIVENESS_OUT` (default `/tmp/leandros-liveness`). Use it for any
  guest workload longer than a few seconds where "it hung" is a possible outcome.

- **Kernel stall diagnostics (2026-09-15):** every CPU counts its local timer
  ticks; a CPU that takes none for 2 s is reported by a live one on the raw UART as
  `[WDOG] cpuN took no timer tick for ~2 s ...: pid=P (/bin/x) last syscall 0x..
  preempt_disable=.. [spinning for LOCK held by cpuM]`, repeated every 10 s, and
  kicked with a reschedule IPI each scan. `pid=0` = the CPU is idle with a dead
  timer (hypervisor lost the vtimer edge — a `[TIMER] cpuN virtual timer silent
  ... re-armed` line follows once the IPI runs its idle loop); `pid=P in syscall`
  = an IRQ-off spin in the kernel, and the lock line names it when it is one of
  RUN_QUEUE / PIPEWIRE_STATE / FD_TABLES / PIPE_RINGS / VIRTIO_GPU / PORT_TABLE /
  EPOLL / ADDRSPACE_BUSY (`sched::lockwatch`). Only a stall of ALL CPUs at once
  prints nothing — then the host `sample` is the instrument.

- **The guest clock used to run slow — 5–23 % under HVF, 12–17 % under
  aarch64 TCG, 1–36 % on x86_64/TCG — FIXED 2026-09-16 (`67c9ba1`).** aarch64
  reloaded `CNTV_TVAL` from *now* inside the tick handler so each tick's
  interrupt latency stretched the period; x86_64 lost LAPIC periods that TCG
  coalesced. Both now keep the tick on an absolute grid with catch-up, and
  `clock_gettime` reads the counter (CNTVCT / TSC) directly. Measure with
  `clockdrift.py <secs> <label>` (same `LEANDROS_RUN_ID`): it prints
  `guest=… host=… ratio=… err=…`; expect |err| < 0.5 % idle over 30 s (a
  10 s window carries ±50 ms of serial/echo offset, i.e. ±0.5 % of noise —
  use 30 s before believing a rate error). Any timing baseline recorded
  before this date (MAME run wall times, `sleep` durations, desktop settle
  times, `[WDOG]` intervals) was measured on the slow clock. On x86_64 the
  boot log prints the TSC frequency every clock derives from, once:
  `[TSC] 1896.002 MHz (cpuid 0x40000010); pit measured 1895.921 MHz` —
  the source in parentheses is CPUID when the CPU/hypervisor states one
  (QEMU only does with `-cpu …,tsc-frequency=<Hz>`), else `pit`. The same
  number is `cpu MHz` in `/proc/cpuinfo`. Kernel `*_us` diagnostics
  (`[SND] TX stalled t_ms`, `[PW] producer gap`, DRMSTAT) were a raw
  `rdtsc/1000` on x86_64 until 2026-09-18 — 1.9× fast on the laptop, 4.5×
  on the 7950X — and are on this clock since.

- **HVF needs a GICv3 machine since QEMU 11.1 (Homebrew, 2026-09-05).** `-accel hvf` on
  `-machine virt,gic-version=2` exits immediately with `HVF does not support GICv2
  emulation` — a launch refusal, not a guest hang, and the reason every aarch64 boot on
  the Mac silently failed between 2026-09-05 and 2026-09-14. Both launchers now pass
  `gic-version=3`; the kernel detects GICv2 vs GICv3 at `gic::init` (serial prints
  `[GIC] ID_AA64PFR0.GIC=… -> GICv3`) and drives either, so the same command line works
  under TCG and KVM. Only the virt build detects — both Pi boards are GIC-400 (GICv2)
  and stay on the memory-mapped path. If a boot dies with zero serial output, read
  `/tmp/leandros-qemu-stderr.log` for this refusal before bisecting the kernel.

- **HVF acceleration history (the direct-boot path is still TCG-only)** — on QEMU 10.x this was an
  outright crash (`Assertion failed: (isv), function hvf_handle_exception, file hvf.c,
  line 1883`). Re-tested and root-caused 2026-07-15 on QEMU 11.0.2: the crash is gone,
  and the hang is NOT in LeandrOS's own boot path — bisected with serial markers through
  every stage of `entry_aarch64.s` (secondary-core park, EL3→EL2→EL1 drop, MMU/page-table
  setup) and into `kernel_main`, all of which complete correctly and fast under HVF. The
  actual hang is inside `arch/aarch64/src/uart.rs::putc()`'s flow-control spin-wait
  (`while rd(FR) & FR_TXFF != 0`): the *second* consecutive UART write blocks forever —
  confirmed permanent (60s wait, byte count never advances), not merely slow, and
  independent of `-smp 1` vs `4` and `highmem=off` vs default. This matches a known,
  currently-unresolved upstream QEMU bug class: QEMU's PL011 model paces TX-FIFO drain
  via a virtual-time timer that doesn't get serviced while an HVF-accelerated vCPU thread
  runs (see [siderolabs/talos#13108](https://github.com/siderolabs/talos/issues/13108) —
  same symptom, "zero console output, zero CPU usage" shortly after boot, on QEMU
  virt+HVF+Apple Silicon, unresolved as of this writing). Not fixable from LeandrOS code —
  would need a QEMU-side fix. The driver intentionally omits `-accel hvf`; TCG is used
  instead.

  **Tried the fix from that Talos issue's actual resolution comment
  (`-machine virt,gic-version=max` instead of a fixed version) — made it WORSE, not
  better.** Hangs immediately after the very first MMIO write (before the secondary-core
  park check even completes), vs. `gic-version=2`'s hang which at least gets deep into
  `kernel_main`. This isn't simply "our GIC driver is GICv2-only, can't talk to GICv3"
  (true — `arch/aarch64/src/gic.rs` has no redistributor/`ICC_*`-system-register support —
  but that gap can't explain a hang this early, since no GIC-touching code runs before the
  park check). Don't attempt a GICv3 port expecting it to fix this without first
  re-verifying with the same raw-UART-marker bisection technique (see
  `project_mame_perf_investigation` memory) — the earlier hang may be a separate, unrelated
  QEMU/HVF bug specific to this GIC-version/MMIO-trap combination.

  See memory `project_mame_perf_investigation` for the full diagnosis. Re-test on
  future QEMU version bumps since the failure mode already changed once (10.x crash →
  11.0.2 hang); if it's ever fixed upstream, HVF would give a large (5-20x class) speedup
  for aarch64 guest workloads like MAME.

  **Everything above is about the direct-kernel-boot path only — still unfixed, still
  needs an upstream QEMU fix.** The UEFI/Limine boot path (EDK2 firmware + Limine
  bootloader — a completely different code path through `entry_aarch64.s`'s
  `limine_entry:` branch) is a **different story: FIXED 2026-07-15, and now the
  default** for `driver.py start aarch64` / `./scripts/run-qemu.sh aarch64` on an Apple
  Silicon host (pass `uefi-tcg` / `--tcg` to opt back into software emulation). It used
  to hard-crash with `assert(isv)` right after PCI-probing the virtio-sound device (full
  firmware/bootloader/kernel-init/PCI-scan completing first — much further than the
  direct-boot hang). Root cause: `drivers/src/virtio_gpu.rs`'s `init_device()`/
  `setup_queue()` wrote `common_cfg`/`notify_cfg` BAR-mapped VirtIO registers through
  plain (non-`volatile`) raw-pointer field assignments — including three adjacent `u64`
  fields (`queue_desc`/`queue_driver`/`queue_device`) that happened to be 8-byte-aligned
  inside the `#[repr(C, packed)]` struct. Because the writes weren't `volatile`, LLVM was
  free to merge/reorder them; it synthesized a wide store (almost certainly an `STP`
  load/store-pair) that QEMU's HVF backend can't decode (`ESR_EL2.ISV` is clear for that
  instruction form) — a real QEMU bug, but triggered by a real LeandrOS bug.
  `drivers/src/virtio_blk.rs` already used the correct
  `core::ptr::addr_of_mut!(...).write_volatile(...)` idiom for the identical struct;
  `virtio_gpu.rs` had strayed from it. Fixed by converting every `virtio_gpu.rs`
  MMIO-register access (including the `notify_addr` kick writes in
  `send_command`/`send_command_raw`) to the same volatile-safe pattern.

  **A second instance of the identical bug was found the same day in
  `drivers/src/virtio_keyboard.rs`** — surfaced by testing `run-qemu.sh` directly, since
  (unlike `driver.py`'s headless command) it includes `-device virtio-keyboard-pci` by
  default and hit the same `assert(isv)` right after `[KBD] Found VirtIO Input device`.
  An initial grep for the non-volatile pattern across the other virtio drivers missed
  this file because it accesses fields via `self.common_cfg` rather than a bare local
  variable — a reminder to re-run that kind of check with a pattern that actually
  matches the file's access style, not just the first one found. Fixed identically
  (same `init_device()`/`setup_queue()` shape, same three-`u64`-field trigger).

  Verified via `lldb` backtrace before each fix (crash squarely inside
  `hvf_arch_vcpu_exec`, not LeandrOS code) and via full boot-to-shell + GPU-framebuffer
  screenshot after, through both `driver.py` and a real `./scripts/run-qemu.sh aarch64`
  run (which exercises virtio-keyboard-pci, virtio-net, virtio-sound, and virtio-gpu
  together). Both architectures' TCG boot path re-verified unaffected. See
  `project_mame_perf_investigation` memory for the full bisection writeup (serial
  markers narrowed the GPU crash from "somewhere in PCI scan" down to the exact
  three-statement window in `setup_queue()`).

- **Socket "Connection refused" on first connect** — QEMU's Unix chardev creates the
  socket file before `listen()` is called. The driver retries for ~6s with 150ms gaps;
  do not check `os.path.exists(sock)` alone as readiness gate.

- **`-audiodev none,id=snd0`** — The VirtIO sound device requires an audiodev; `none`
  is a valid backend that discards audio. Without it, QEMU errors on the
  `-device virtio-sound-pci` line. Use `LEANDROS_AUDIO_WAV` (see Audio capture above)
  to capture instead.

- **QEMU 11.x permanently stalls a virtio-snd stream whose queue it ever polls
  empty** — the split audio backend loses its frontend-refill wakeup on the first
  empty poll (one-shot): every control command still returns OK, TX buffers are
  accepted but never complete, capture stays empty. All backends (wav/none/
  coreaudio) affected. The guest self-heals with three mechanisms
  (`drivers/src/snd.rs` + `servers/pipewire/src/lib.rs`): a 100 Hz tick pump
  (sched::register_tick_hook) that drains the spool and pads the ring with
  silence below a watermark; the same padding from inside a blocked producer
  push (the pump can't take the lock then); and a 250 ms frozen-used-index
  stall detector that restarts the stream. A few `[SND] TX stalled …
  recovering stream` lines during an app's silent startup phase are expected
  and inaudible; recoveries during audible playback are a regression.
  **Do not shrink the audio buffers below SPOOL_BYTES=64 KiB /
  buffer_bytes=65536 / natural ring depth**: QEMU's first poll after START
  gulps up to buffer_bytes at once, its timer slips multiply per-tick demand,
  and configurations below this line are bimodally unstable (pass some boots,
  death-spiral others — validated over ~25 test runs on 2026-07-18, aarch64
  HVF). Steady-state audio latency is ~425 ms (10.5 KiB ring + 64 KiB spool,
  both kept full by backpressure). **QEMU 11.1+ enforces the device's 64-entry
  queues** (11.0 silently accepted our 256), so the TX ring holds 21 × 512 B;
  serial prints `TX ring full (first time), submitted=0x15`. **Every TX buffer
  must be whole frames** (traced 2026-09-15): QEMU's audio core writes nothing
  for a sub-frame remainder, virtio-snd reads the 0 as "backend full" and holds
  that buffer forever — the stream freezes with a full ring until the detector
  restarts it, ~30×/min in MAME. `send_pcm_data` rounds down to `channels×2`.
  Re-test with `audio-glitch-test.sh <label> [arch] [--no-build]` (60 s MAME;
  count `recovering stream` + `producer gap` lines — the wav zero-run metric is
  blind to stalls because QEMU's wav backend writes nothing while a stream is
  released) on QEMU upgrades, and check `/tmp/leandros-qemu-stderr.log` for
  `exceeds max size`.

- **AArch64 UEFI outputs VT100 cursor codes on serial** — UEFI and Limine use
  `\e[row;colH` cursor positioning and `\e[K` erase sequences. The serial log contains
  raw bytes; the driver does not strip these (they appear cleanly on a real terminal
  or in the framebuffer screenshot).

- **Monitor line-editing noise** — QEMU monitor echoes each character with `[K[D`
  cursor-movement sequences. `driver.py` strips these with `_strip_ansi()` so monitor
  responses are readable.

- **`-device virtio-keyboard-pci` is now a default device (both arches)** — It used
  to be omitted because it silently hung QEMU on this machine. That was NOT a QEMU
  bug: it was the same non-volatile-MMIO driver bug fixed in `virtio_gpu.rs` on
  2026-07-15, present a third time in `drivers/src/virtio_keyboard.rs`'s
  `init_device()`/`setup_queue()` (the identical wide-store trigger, which QEMU's HVF
  backend can't decode). With that fixed, the device boots cleanly and delivers real
  input. Verified 2026-07-21 end-to-end: booted aarch64 (HVF) with the keyboard
  present (`[KBD] Found VirtIO Input device` / `initialized`), ran
  `mame captcomm -rompath /` to its "Press any key to continue" info screen, and
  injected keys via the QEMU monitor (`sendkey ret`/`5`/`1` over the monitor socket —
  `sendkey` routes through the attached virtio-keyboard as genuine guest input
  events). MAME advanced past the info screen, registered a coin (CREDIT 1), and
  reached the PLAYER SELECT screen — i.e. QEMU → virtio-keyboard → LeandrOS evdev
  input stack → MAME OSD input layer works, distinct keys included. The shell is
  still driven through the serial socket; the keyboard is what real interactive
  apps (MAME, doom) consume. To inject a key headlessly: send `sendkey <qcode>` to
  the monitor socket (e.g. via `driver._monitor_send('sendkey ret')`).

- **x86_64 requires no pflash vars file** — unlike AArch64 which needs `aarch64_vars.fd`
  for UEFI variable storage, x86_64 OVMF works with a single `OVMF_CODE.fd` image.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `HVF assertion failed` | Do not add `-accel hvf`; TCG is the correct mode here |
| `serial socket did not appear` | Check QEMU stderr: `2>/tmp/qemu-err.log`; usually a missing firmware path |
| Shell prompt never appears | Run `driver.py log` to check for kernel panics |
| `screendump` produces 0-byte file | VirtIO GPU not initialized; check boot log for `[GPU] VirtIO GPU initialized` |
| `cmd` returns empty output | Shell may have exited; run `start` again |
