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

# 2. Send a shell command, get output
python3 .claude/skills/run-leandros/driver.py cmd "help"
python3 .claude/skills/run-leandros/driver.py cmd "ls /bin"
python3 .claude/skills/run-leandros/driver.py cmd "info"

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

## Build

```sh
./scripts/build-all.sh              # both architectures (aarch64 + x86_64)
./scripts/build-all.sh --arch aarch64   # one arch only
```

Always build **release** targets — debug builds crash during early boot (see CLAUDE.md).
Build time: ~3–5 minutes clean, ~30s incremental.

## Gotchas

- **HVF acceleration doesn't boot LeandrOS on Apple Silicon** — on QEMU 10.x this was an
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
  is a valid backend in QEMU 10.x that discards audio. Without it, QEMU errors on the
  `-device virtio-sound-pci` line.

- **AArch64 UEFI outputs VT100 cursor codes on serial** — UEFI and Limine use
  `\e[row;colH` cursor positioning and `\e[K` erase sequences. The serial log contains
  raw bytes; the driver does not strip these (they appear cleanly on a real terminal
  or in the framebuffer screenshot).

- **Monitor line-editing noise** — QEMU monitor echoes each character with `[K[D`
  cursor-movement sequences. `driver.py` strips these with `_strip_ansi()` so monitor
  responses are readable.

- **`-device virtio-keyboard-pci` omitted** — The keyboard device caused QEMU 10.x
  to silently hang on this machine; removed from the driver command. The shell is
  driven entirely through the serial socket so the keyboard is not needed.

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
