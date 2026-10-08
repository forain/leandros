# Development workflow

How LeandrOS is developed day to day: which machine does what, how parallel work is organised into lanes, how QEMU is driven from scripts, and how to keep build outputs from lying about which code is running.

## Machine roles

Development uses several machines, chosen by **accelerator**, not by architecture:

| Machine | Accelerators | Role |
|---|---|---|
| A Mac (Apple Silicon) | AArch64/HVF (fast), x86-64/TCG (slow) | Primary development box; every change is built and tested on both architectures here. A source-built GPU QEMU gives the guest virgl over ANGLE (GLES 3.1). |
| A Linux desktop with an AMD GPU | x86-64/KVM (fast), AArch64/TCG (slow) | The real-GPU host: Venus/Zink and virgl reach a real Vulkan/OpenGL driver (RADV/radeonsi). The machine for `--venus` and GPU performance work. |
| A Linux laptop with an Intel iGPU | x86-64/KVM, AArch64/TCG | A second KVM host with a different GPU vendor (Venus over ANV); small, so one QEMU at a time. Useful for code-and-build lanes. |
| A Raspberry Pi 5 | real Cortex-A76 | Hardware bring-up, over the 3-pin UART debug connector ([Raspberry Pi 5](../hardware/raspberry-pi-5.md)). |
| A real x86-64 PC | real hardware | USB-stick boots, USB HID, the NVIDIA work ([PC hardware](../hardware/pc.md)). |

The rule that follows from the table: **fast x86-64 work goes to a Linux machine, fast AArch64 work to the Mac, host-GPU work to the Linux desktop**, and a result is always quoted with its accelerator. The Mac and a Linux PC are mirror images — what looks "AArch64-only" on one is "slow-target-only", and reproduces on x86-64 on the other ([methodology](methodology.md#the-accelerator-is-not-the-architecture)).

The repository moves in one direction: commits go from the development machine to the shared remote, and each other machine fast-forwards (`git pull --ff-only`). The out-of-tree staging directory and the sibling repositories are synced separately (below).

## Lanes and waves

Parallel work is organised into **lanes**. A lane is one question or one fix, with:

- its own branch, `lane/<name>`, branched from `main`;
- its own worktree, **a sibling of the repository root** (`../leandros-<name>`), never nested inside it. The build resolves `../brush`, `../relibc`, `../crossterm`, `../coreutils`, `../doomgeneric` relative to the tree, and a nested worktree silently produces an image without a shell;
- its own `LEANDROS_RUN_ID`, so its QEMU instances do not collide with anyone else's (below);
- a declared set of files it owns, with overlaps with other lanes named up front — two lanes once fixed the same page-table leak independently because the overlap was not named;
- a **lane note**, `artifacts/notes/lane-<name>-<date>.md`, recording the outcome (FIXED, PARTIAL or **REFUTED** — a refuted premise is a valid and common result), the commits, the evidence per architecture and accelerator with numbers, and what is left open.

A **wave** is a set of lanes started from the same `main` and merged together. Lanes are merged one at a time into an integration branch, which is then rebuilt and verified on both architectures before `main` moves: each lane compiling alone does not mean they compile together (one wave caught two cross-lane compile breaks this way — a new enum variant against another lane's exhaustive `match`). Integration verification runs the regression suites on fresh images, a graphical session on every GPU path, and the scenario checks (VT switching, power-off and restart, privilege tests), and records a GO/NO-GO per architecture. Each wave ends with a reconciliation of [TODO.md](../../TODO.md) and a wave note (`artifacts/notes/wave-<date>.md`).

On a shared machine, only stop QEMU processes you started (by PID). Note that `pkill -f qemu-system-aarch64` run over ssh matches, and kills, the ssh command itself; use a pattern such as `qemu-system-aarch6[4]`.

## The QEMU driver

`run-qemu.sh` is the interactive path ([running](../getting-started/running.md)). [`.claude/skills/run-leandros/driver.py`](../../.claude/skills/run-leandros/driver.py) is the scriptable one: it starts QEMU headless with the serial console and the monitor on Unix sockets and a QMP socket, and exposes subcommands:

| Command | Does |
|---|---|
| `start [aarch64\|x86_64] [mode] [--venus\|--virgl]` | Boot and wait for the serial prompt. `mode` is `uefi` (HVF on Apple Silicon), `uefi-tcg`, `uefi-hvf`, `direct` or `raspi4b`. Without a GPU flag the guest gets plain virtio-gpu — the headless-test path, with no greeter burning CPU. |
| `login <user> <password>` | Log in on the serial console; waits for a real shell prompt with kernel lines stripped, exits 2 if none appears |
| `cmd "<command>" [timeout]` | Send a shell command, print its output |
| `run "<command>" [timeout] [arch]` | Run a command and report its **exit status** reliably: prints the output, then `RC=<n> status=<ok\|rc-file\|timeout\|lost>`, and exits with the command's status (124 timeout, 125 lost) |
| `session <step_s> <cmd>…` | Pump one command per step over one held serial connection |
| `screenshot [out.ppm]` | Capture the display (PPM, converted to PNG on macOS); over VNC for GL scanouts, which have no surface for `screendump` |
| `chord <qcode>…`, `wheel <dir> [n] [x y]`, `qmp <cmd> [json]` | Hold a key chord (e.g. `ctrl alt f2`), scroll, or send raw QMP |
| `log`, `status`, `stop` | Serial log, liveness, clean shutdown |

**Never parse `cmd "<x>; echo RC=$?"` for a status.** Kernel diagnostics reach the UART unsynchronised with tty output and can land inside the `RC=` line, and a test that prints `-> ` and pauses looks like a prompt and ends the read mid-test. `run` frames the command with a nonce sentinel (`<<LRC:nonce:rc>>`, matched with kernel lines cut out), reads until that sentinel only, falls back to a status file in the guest, and types the command only after its echo is seen intact.

Waits scale with the accelerator: ×3 on TCG, ×1 on HVF/KVM (`LEANDROS_WAIT_SCALE` overrides). QEMU drops serial output while no client is attached, so `log` shows only what was read during a connection; `/tmp/leandros[-<RUN_ID>]-serial-full.log`, QEMU's own log file, has everything since `start`. QEMU's stderr goes to `/tmp/leandros-qemu-stderr.log` — read it first when a boot produces zero serial output (a launch refusal such as HVF's GICv2 error lands there).

### Driver environment variables

| Variable | Effect |
|---|---|
| `LEANDROS_RUN_ID` | Scopes every socket, PID file, log and image path the driver owns to a name, so several QEMUs (one per lane) run on one machine without colliding. Every call in a lane must carry the same value. Unset = the default paths. |
| `LEANDROS_VNC_PORT` | Which VNC listener a GL capture connects to (default 5909), for machines running more than one GPU session |
| `LEANDROS_WAIT_SCALE` | Override the accelerator wait multiplier |
| `LEANDROS_QEMU_EXTRA` | Extra QEMU arguments (shell-split), e.g. `-trace enable=virtio_snd_*,file=…` |
| `LEANDROS_AUDIO_WAV=<file>` | Record everything the guest plays through virtio-sound into a WAV (44.1 kHz stereo S16; the header is finalised when QEMU exits) |
| `LEANDROS_QEMU_MEM`, `LEANDROS_SMP` | Guest RAM; CPU topology (`LEANDROS_SMP=1` rules out SMP races) |
| `LEANDROS_TCG_THREAD=single\|multi`, `LEANDROS_X86_CPU` | TCG threading; the x86-64 TCG CPU model |
| `LEANDROS_USB_STICK=<img>[,…]`, `LEANDROS_USB_INPUT` | Boot from USB sticks; USB HID input |
| `LEANDROS_GPU`, `LEANDROS_QEMU_PREFIX`, `LEANDROS_NET`, `LEANDROS_MAC` | As for `run-qemu.sh` |

### Tools built on the driver

All in [`.claude/skills/run-leandros/`](../../.claude/skills/run-leandros) unless noted:

| Tool | Purpose |
|---|---|
| `runtests.py <arch> <tag> [--suite regress\|net\|drm] [--gpu …] [--repeat N]` | Boot, log in as root, run the suites; one `=== cmd: RC=n [status, secs] fails=k` line each, `summary.json`; exit 0 iff all passed ([testing](testing.md)) |
| `liveness-run.py <label> "<cmd>" [--wav] [--timeout S]` | Run one guest command with a persistent timestamped serial reader, a userspace heartbeat, optional WAV-growth tracking and — only on a stall — a host-side sample of the QEMU process. Use it for any workload where "it hung" is a possible outcome. |
| `clockdrift.py <secs> <label>` | Guest clock against host wall time: `guest=… host=… ratio=… err=…`. Expect \|err\| < 0.5 % idle; use 30 s windows, since a 10 s window carries ±0.5 % of serial noise. |
| `ffsession.py`, `doomsession.py`, `coldstart-bench.py` | Scripted desktop sessions: greeter login, `cosmic-term`, Firefox; DOOM under COSMIC; app and dialog launch latency (VNC-polled, ±0.3 s) |
| `gpu-session-loop.py <n> <tag> [virgl\|venus]` | Soak N sessions and count host GPU faults per boot |
| `audio-glitch-test.sh`, `coreutils-test.py`, `magcount.py` | Audio stall detection; the coreutils conformance run; counting undrawn-texture (magenta) pixels in a capture |
| [`scripts/zinkbench.py`](../../scripts/zinkbench.py) | Boot a `--venus` guest, run a fixed input script, report fps/flip counters, host QEMU thread CPU and a VNC photograph — the tool behind the Zink performance numbers |
| [`scripts/scmrun.py`](../../scripts/scmrun.py) | Persistent serial reader for a long command, with an optional completion marker |
| `scripts/greeterstorm.py`, `scripts/vnclat.py`, `scripts/gpuirqbench.py` | Greeter respawn-storm leak measurement, VNC input latency, GPU interrupt benchmarking |

Every tool takes its paths from `LEANDROS_RUN_ID`, so it can run beside other lanes. [`artifacts/`](../../artifacts) holds the older, milestone-specific harnesses that earlier claims were measured with.

## Build outputs and image provenance

Images are gitignored build outputs (several GiB per tree), and a QEMU boots whatever is on disk. The pitfalls in [building](../getting-started/building.md#image-provenance-pitfalls) apply; on top of them, for parallel work:

- **Build outputs are shared state that file ownership does not protect.** Partitioning lanes by file works for source; it does nothing for `leandros-limine-*.img` and `f2fs-data*.img` in a shared tree. Each lane builds in its own worktree; to test without rebuilding, clone images with a copy-on-write copy (`cp -c` on APFS, `cp --reflink` on btrfs/XFS), which is instant and free.
- **Shared siblings.** `../doomgeneric` and the other siblings are one directory per machine for every worktree; the build isolates object directories per worktree and architecture, but a binary left in a sibling belongs to whichever tree linked it last.
- **The out-of-tree staging directory diverges per machine.** The image builder reads prebuilt COSMIC binaries, the Mesa GPU stack, session data, icons and the wallpaper from `~/code/leandros-artifacts`, which is gitignored and hand-synced. When two machines disagree about what the desktop looks like, diff these trees before suspecting the architecture. On 2026-08-09 "full desktop on AArch64, only a clock on x86-64" was three per-machine gaps in that directory (a whole data subtree shadowing the repository's, three missing applet binaries, ten missing icons) with zero architecture involvement. Two recurring shapes: a directory that exists at all can shadow the repository's copy (now merged per file), and "outputs synced, machinery never was" — binaries present without the sources and scripts that built them. Compare manifests with `LC_ALL=C` on both `sort` and `comm`; a locale-collating `comm` reports files missing from both sides.
- **Re-generate before debugging up-stack.** A file system that mounted is not a file system that is intact: when mounts start failing, regenerate the images (and check a pristine `git archive HEAD` kernel fails identically) before blaming code.

## See also

- [Testing](testing.md) — the suites and how to run them.
- [Debugging](debugging.md) — what to do when a run stalls or goes silent.
- [Methodology](methodology.md) — how claims are established.
- [Conventions](conventions.md)
