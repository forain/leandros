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

# 1. Boot (defaults to aarch64; takes ~5s on this machine)
python3 .claude/skills/run-leandros/driver.py start aarch64
python3 .claude/skills/run-leandros/driver.py start x86_64

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
```

This is useless headless and blocks the terminal; use the agent path above.

## Build

```sh
./scripts/build-all.sh              # both architectures (aarch64 + x86_64)
./scripts/build-all.sh --arch aarch64   # one arch only
```

Always build **release** targets — debug builds crash during early boot (see CLAUDE.md).
Build time: ~3–5 minutes clean, ~30s incremental.

## Gotchas

- **HVF acceleration crashes QEMU 10.x on Apple Silicon** — `Assertion failed: (isv),
  function hvf_handle_exception, file hvf.c, line 1883`. The driver intentionally omits
  `-accel hvf`. TCG (software emulation) is used instead and is fast enough (~5s boot).

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
