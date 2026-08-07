# M6 session-binary load-smoke results

**Lane:** host-only, repo-read-only. Workdir `~/code/leandros-artifacts/m6-loadsmoke/`.
**Method:** Alpine 3.21 containers (Docker Desktop), aarch64 native / x86_64 emulated
(`--platform linux/amd64`, qemu-user under the hood — same pattern as `llvmpipe-lane`).
Each binary invoked by calling our **own staged musl `libc.so` directly as the dynamic
loader** (musl's libc.so-is-also-ld.so trick):

```
env -i PATH=/usr/bin:/bin [XDG_RUNTIME_DIR=<tmp>] \
  /art/m6-loadsmoke/libs-<arch>/libc.so --library-path /art/m6-loadsmoke/libs-<arch> <binary>
```

`env -i` strips all inherited environment (no `LD_LIBRARY_PATH` leak, no Alpine anything);
resolution is confined to `--library-path`, so Alpine's own `/usr/lib` is never consulted.
6-second watchdog per invocation (background + `sleep 6; kill -KILL`).

## Runtime lib set staged (`m6-loadsmoke/libs-<arch>/`)
- `libc.so` — from `m3-gl-stack/sysroot-<arch>/usr/lib/libc.so` (also serves as the loader)
- `libxkbcommon.so.0`, `libudev.so.1`, `libseat.so.1`, `libinput.so.10`, `libevdev.so.2`,
  `libmtdev.so.1`, `libpixman-1.so.0`, `libdisplay-info.so.3` — from `m4-input-ship/<arch>/usr/lib/`
  (freshest builds; superset of what these binaries actually need, harmless to over-provide)
- `libpipewire-0.3.so.0` (+ unversioned symlink) — the stub from `pipewire-gap/lib/<arch>/`

Per the M6 manifest, actual link-time `DT_NEEDED` per binary is small (subset of
`{libc.so, libxkbcommon.so.0, libudev.so.1}`, plus the pipewire stub for the daemon only) —
everything Wayland/EGL is `dlopen`'d at runtime, out of scope for a link-closure smoke.

## Result table

| Binary | x86_64 | aarch64 | Reached / verdict |
|---|---|---|---|
| cosmic-session | **LOADS-CLEAN** | **LOADS-CLEAN** | Loader resolves everything, reaches `main`, tokio runtime starts, fails cleanly in `zbus` D-Bus connect (`I/O error: No such file or directory`, `src/main.rs:83`) — expected: no session/system bus socket in the container. Identical on both arches. |
| cosmic-panel | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`; logs `WARN Falling back to default panel configuration` / `ERROR Panel Entry Error: NoConfigDirectory` / `ERROR failed to create workspaces dbus proxy` / `ERROR Failed to connect to the notifications daemon`, then exits rc=101 (clean, non-fatal to the loader — all app-level, expected without D-Bus/config). Identical both arches. |
| cosmic-notifications | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`; `WARN Failed to connect to journald`, `WARN` font/locale fallbacks, then **panics with a clean message** `Create event loop: NotSupported("neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set")` at `iced/winit/src/lib.rs:93` — exactly the expected-GOOD "no compositor" outcome named in the task. rc=101 both arches. |
| cosmic-bg | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`, fails cleanly: `Error: wayland client connection failed / Could not find wayland compositor` (`src/main.rs:99`), rc=1. Expected-GOOD. Identical both arches. |
| cosmic-osd | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`, same winit `NotSupported(...WAYLAND_DISPLAY...)` panic as cosmic-notifications, but this crate's panic hook **aborts** (rc=134/SIGABRT vs. 101) after printing the identical clean message. x86_64 log additionally shows qemu-user's own `qemu: uncaught target signal 6 (Aborted)` — an artifact of signal trapping under emulation, not a new failure mode. Expected-GOOD (intentional abort-on-panic hook, not a raw crash — full diagnostic message printed first). |
| cosmic-launcher | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`; `WARN Failed to connect to dbus`, then the same winit `NotSupported` panic, rc=101 (unwind, not abort). Identical both arches. |
| cosmic-applibrary (cosmic-app-library) | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`, same winit `NotSupported` panic, rc=101. Identical both arches. |
| cosmic-settings | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`, same winit `NotSupported` panic via its own panic-hook wrapper (`The application panicked (crashed)` + message + location), rc=134/SIGABRT (abort-on-panic hook, same shape as cosmic-osd). Identical both arches; also the only other binary needing `libudev.so.1` — resolved cleanly. |
| cosmic-settings-daemon (+ pipewire stub) | **LOADS-CLEAN** | **LOADS-CLEAN** | Reaches `main`. Bare-env: first a **caught** panic `runtime dir required by varlink service` (`varlink-server/src/lib.rs:28`), then the process continues and dies on `src/wayland.rs:28:45: unwrap() on Err(NoCompositor)`, rc=101. With `XDG_RUNTIME_DIR` set: the varlink panic disappears (runtime dir now present) and it dies directly on the same wayland `NoCompositor` unwrap, rc=101. **Both outcomes are expected-GOOD** — no symbol errors, no missing `.so`, `libpipewire-0.3.so.0` stub resolves silently. See pipewire-stub caveat below. |

**18/18 (9 binaries × 2 arches) = LOADS-CLEAN. Zero BROKEN findings.** No symbol lookup
errors, no missing `.so`, no segfaults (rc=139), no abort in static initializers anywhere.
The two rc=134 cases (cosmic-osd, cosmic-settings) are intentional abort-on-panic hooks that
print a full clean diagnostic first, not raw crashes — flagged here for transparency but not
a defect.

## cosmic-settings-daemon / pipewire-stub-specific note (task item 3)
The daemon's `main()` sets up its Wayland client connection **synchronously and
unconditionally before** spawning the `cosmic-pipewire` worker thread (see
`src/wayland.rs:28`, an `.unwrap()` on the Wayland connect result). In this compositor-less
container smoke, the process therefore dies on that Wayland unwrap **before** it ever reaches
`cosmic_pipewire::run()` — so the quadratic-backoff pipewire retry loop described in
`notes/pipewire-gap-design.md` §2/§6 is **not independently re-observed at runtime here**.
What IS confirmed: `libpipewire-0.3.so.0` (stub) is present in `DT_NEEDED`, resolves with
zero unresolved `pw_*`/`spa_*` symbols, and never causes a load-time or early-main failure —
consistent with the design doc's own on-target checklist (§6), which already notes this
requires a real compositor to fully exercise. `DBUS_SESSION_BUS_ADDRESS` was never reached
either (Wayland unwrap fires first); no daemon behavior depends on it before that point in
this environment.
**Landmine worth flagging to the M6 orchestrator:** unlike the pipewire path (graceful
`CreationFailed` → backoff retry, per design doc), the daemon's Wayland-connect failure path
is a hard `.unwrap()` → process abort, not a retry. This is a non-issue on real target (a
compositor is expected to be up per the M6 exit criterion) but means a compositor-less
container/CI smoke can never observe the daemon reach steady state — a fake/stub Wayland
socket would be needed to go further, which is out of scope for this smoke pass.

## Repro
```
cd ~/code/leandros-artifacts
docker run --rm --platform linux/arm64 -v "$PWD:/art" alpine:3.21 sh /art/m6-loadsmoke/run-in-alpine.sh aarch64
docker run --rm --platform linux/amd64 -v "$PWD:/art" alpine:3.21 sh /art/m6-loadsmoke/run-in-alpine.sh x86_64
```
Raw output: `m6-loadsmoke/results/raw-<arch>.txt` (both `bare-env` and `xdg-runtime-dir`
modes, per binary, with `##### END rc=N #####` trailers). Driver script:
`m6-loadsmoke/run-in-alpine.sh`. Staged lib dirs: `m6-loadsmoke/libs-<arch>/`.

## Missing-lib gaps
**None found.** Every binary's transitive closure resolved fully from
`m3-gl-stack` (libc.so) ∪ `m4-input-ship` (libxkbcommon/libudev/libseat/etc.) ∪
`pipewire-gap` (libpipewire stub, daemon only) — matching the M6 manifest's link-time
analysis exactly. No new ship-set gap surfaced by this dynamic smoke beyond what static
analysis (manifest + `verify-closure.sh`) already predicted.
