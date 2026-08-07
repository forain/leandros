qui 23 jul 2026 15:11:38 -03 - starting aarch64 loadsmoke run (native)

## Setup
- Staged runtime libs at `m6-loadsmoke/libs-{x86_64,aarch64}/`: libc.so (from
  m3-gl-stack/sysroot-<arch>, doubles as the musl loader), libxkbcommon/libudev/libseat/
  libinput/libevdev/libmtdev/libpixman-1/libdisplay-info (from m4-input-ship/<arch>, freshest),
  libpipewire-0.3.so.0 stub (from pipewire-gap/lib/<arch>).
- Driver: `m6-loadsmoke/run-in-alpine.sh` — invokes each binary via `env -i ... libc.so
  --library-path <libdir> <binary>`, two modes (bare-env, XDG_RUNTIME_DIR=tmp/no
  WAYLAND_DISPLAY), 6s watchdog per invocation, raw output to
  `m6-loadsmoke/results/raw-<arch>.txt`.
- Sanity check first (cosmic-session, aarch64, manual invocation): confirmed loader resolves
  cleanly, reaches main, fails in zbus D-Bus connect — validated the harness shape before
  running the full 9-binary x 2-mode x 2-arch matrix.

## Runs
- aarch64 (native, `--platform linux/arm64`): completed in one shot, all 9 binaries x 2 modes,
  rc=0 driver exit.
- x86_64 (emulated, `--platform linux/amd64`): completed in one shot, identical behavior to
  aarch64 modulo qemu-user's own "uncaught target signal 6 (Aborted)" noise on the two
  abort-on-panic binaries (cosmic-osd, cosmic-settings) — cosmetic, not a new failure.

## Result
18/18 (9 binaries x 2 arches) = LOADS-CLEAN. Zero BROKEN. No symbol lookup errors, no
missing .so, no segfaults, no abort-in-static-init anywhere. Two binaries (cosmic-osd,
cosmic-settings) abort (rc=134) via an intentional panic-hook after printing a full clean
diagnostic — flagged as a note, not a defect. cosmic-settings-daemon's Wayland-connect
`.unwrap()` fires before the pipewire worker thread is ever spawned, so the pipewire
retry-loop design (pipewire-gap-design.md) could not be independently re-observed at runtime
in this compositor-less container — flagged as a landmine for the M6 orchestrator (needs a
stub Wayland socket to go further; out of scope for this smoke).

DONE. Results doc: notes/m6-loadsmoke-results.md. STOPPING per task instructions.
