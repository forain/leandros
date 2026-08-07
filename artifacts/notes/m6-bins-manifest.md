# M6 COSMIC session-bins manifest — FINAL

HOST-ONLY, repo-read-only lane. Workdir: `~/code/leandros-artifacts/m6-session-bins/`.
Foundation reused unmodified: `m3-gl-stack/sysroot-{x86_64,aarch64}` (read-only reference),
own toolchain copy (`m6-session-bins/toolchain`, copied verbatim from `m3-gl-stack/toolchain`),
own `.cargo/config.toml` per crate (`gen-cargo-config.sh`) wiring `zig ld.lld` against the m3
sysroots exactly per the recipe that linked anvil/cosmic-comp (`-C relocation-model=pic -C
target-feature=-crt-static -C link-self-contained=no` + explicit `--dynamic-linker
/lib/ld-musl-<arch>.so.1 -pie` link-args ⇒ ET_DYN + PT_INTERP). Sources: unmodified checkouts
from `/Users/forain/code/cosmic-epoch/<crate>` (already-initialized git submodules, epoch-1.3.0),
rsynced (excluding `.git`) into `m6-session-bins/src/<crate>` — **no source patches applied to
any crate**; only Cargo feature flags used. All 16 produced binaries (8 crates × 2 arches, minus
1 blocked) verified with `llvm-readelf` (ET_DYN, correct `PT_INTERP`, correct `e_machine`) and
`verify-closure.sh` (transitive `DT_NEEDED` resolved against
`m5-session-ship ∪ m4-input-ship ∪ m3-gl-stack/sysroot-<arch>`).

## Final status table

| # | Binary | x86_64 | aarch64 | Feature flags | NEEDED (link-time) | Notes |
|---|--------|--------|---------|----------------|---------------------|-------|
| 1 | cosmic-session | **LINKED** | **LINKED** | `--no-default-features` (drops `logind` default → no `zbus_systemd`/`logind-zbus`/`tracing-journald`) | libc.so | Pure-Rust deps; zbus is pure-Rust D-Bus (no libdbus). Cleanest of the whole ladder. |
| 2 | cosmic-settings-daemon | **BLOCKED** | not attempted | none exists to gate it off | n/a | Hard dep on `libpipewire-0.3` via `audio-server`→`cosmic-pipewire`→`pipewire-sys`/`libspa-sys`. Not staged anywhere. No feature flag in the dependency chain to disable it. See "Blocker" below. |
| 3 | cosmic-panel | **LINKED** | **LINKED** | default features (built `cosmic-panel-bin` workspace member directly) | libxkbcommon.so.0, libc.so | `use_system_lib`/`client_system` do NOT force DT_NEEDED — wayland-sys/EGL still `dlopen`'d at runtime (verified via `strings`). |
| 4 | cosmic-notifications | **LINKED** | **LINKED** | default features (`systemd` → tracing-journald only) | libxkbcommon.so.0, libc.so | libcosmic winit+wayland+multi-window+a11y+dbus-config all resolve with no new DT_NEEDED vs. cosmic-panel. |
| 5 | cosmic-bg | **LINKED** | **LINKED** | `--no-default-features` (drops `avif` default — dav1d-sys not staged, see Blockers) | libc.so | Smallest stretch binary; no wayland/xkbcommon dep at all in its own direct closure. |
| 6 | cosmic-osd | **LINKED** | **LINKED** | default features | libxkbcommon.so.0, libc.so | `logind-zbus` dep is pure-Rust D-Bus, no libsystemd C lib. |
| 7 | cosmic-launcher | **LINKED** | **LINKED** | default features (`default = []`) | libxkbcommon.so.0, libc.so | No gates needed. |
| 8 | cosmic-app-library (`cosmic-applibrary`) | **LINKED** | **LINKED** | default features incl. `wgpu` | libxkbcommon.so.0, libc.so | `wgpu` feature adds no new DT_NEEDED — GPU backend loader is dlopen-based too. |
| 9 | cosmic-settings | **LINKED** | **LINKED** | `--no-default-features --features a11y,linux,single-instance,wgpu,systemd` (keeps every default except `avif`, which pulls dav1d-sys — same gap as #5) | libudev.so.1, libxkbcommon.so.0, libc.so | Only binary needing libudev (page-networking/page-bluetooth). `page-sound`→`cosmic-settings-audio-client` confirmed pure-Rust IPC client — **no libpipewire dependency in this binary**, unlike #2's daemon side. |

**8 of 9 target binaries LINKED clean on both architectures. The only failure is
cosmic-settings-daemon (#2), blocked by a missing system library with no way to feature-gate
around it. All 16 resulting binaries verified: ET_DYN, correct PT_INTERP
(`/lib/ld-musl-<arch>.so.1`), correct `e_machine` (EM_X86_64 / EM_AARCH64), closure CLOSED.**

## Blockers (binaries that did not link)

### cosmic-settings-daemon — BLOCKED, both arches (aarch64 not attempted)
- Invocation attempted: `cargo +nightly build --release --target x86_64-unknown-linux-musl`
  (no feature flags exist anywhere in the chain to gate this off).
- Failure:
  ```
  Package 'libpipewire-0.3' not found
  The system library `libpipewire-0.3` required by crate `libspa-sys` was not found.
  ```
- Root cause: dependency chain `cosmic-settings-daemon` (main bin crate, non-optional) →
  `cosmic-settings-audio-server` (path dep, non-optional) → `cosmic-pipewire` (path dep,
  non-optional) → `pipewire-sys`/`libspa-sys` (build.rs pkg-config probe). **No `[features]`
  gate exists anywhere in this chain** — `cosmic-settings-daemon/Cargo.toml` has no
  `[features]` section at all, and `audio-server/Cargo.toml` depends on `cosmic-pipewire`
  unconditionally.
- `libpipewire` (+ `libspa`) has never been staged in `m3-gl-stack`, `m4-input-ship`,
  `m5-session-ship`, or on the host. This is a genuine closure gap, not a source-patch or
  feature-flag problem.
- Per lane scope (cross-build against the *existing* foundation, no source patches), fixing
  this requires a brand-new pipewire+libspa musl cross-build port — out of scope here.
  STOPPED per instructions; aarch64 not attempted (identical dependency graph, guaranteed
  identical failure).

## Closure gaps found (libraries nobody has staged)
1. **libpipewire-0.3 (+ libspa)** — blocks cosmic-settings-daemon outright (no feature escape
   hatch). The only binary in the whole ladder this actually blocks.
2. **libdav1d (AV1 decoder)** — required by `image`'s `avif-native` feature, which is a
   *default* Cargo feature on both `cosmic-bg` (`avif`) and `cosmic-settings` (`avif`, part of
   its `default` set). Unlike libpipewire, this one IS escapable: both crates linked cleanly
   with `--no-default-features` (re-enabling every other default for cosmic-settings) since
   AVIF wallpaper/image decoding is optional, non-core functionality. Recorded as a gap because
   it silently reduces feature-completeness (no AVIF support in the shipped binaries) rather
   than blocking the binary outright.

No other missing `.so` or `.pc` surfaced across all 9 attempted binaries.

## New landmines
1. **cosmic-settings-daemon ⇒ libpipewire-0.3 hard dependency, unconditional, no feature
   gate** — blocks the whole binary until pipewire+libspa are cross-built and staged. Highest-
   priority follow-up if #2 is ever needed.
2. **`avif` default Cargo feature ⇒ dav1d-sys ⇒ libdav1d, on two different crates**
   (`cosmic-bg`, `cosmic-settings`) — both escapable via `--no-default-features` (re-adding
   whatever other defaults are wanted). Watch for this exact shape (`image`-crate consumer +
   default `avif` feature) on any future binary in this family (e.g. cosmic-files if ever
   attempted).
3. **`use_system_lib`/`client_system` Cargo features do not guarantee DT_NEEDED for
   Wayland/EGL** — wayland-sys and smithay's EGL ffi both fall back to `dlopen` regardless;
   confirmed via `strings` on cosmic-panel's binary (dlopen fallback error strings for
   `libwayland-client.so`, `libwayland-server.so`, `libwayland-egl.so`, `LibEGL` are baked in
   verbatim). Confirm actual link-time closure with `llvm-readelf -d` + `strings` rather than
   inferring runtime library requirements from Cargo feature names alone. Matches the already-
   documented anvil/cosmic-comp EGL-dlopen behavior in `m3-gl-stack/NOTES.md`; now confirmed to
   extend to libwayland-client/-server for panel/notifications/bg/osd/launcher/applibrary/
   settings too (none of the 8 linked binaries has any wayland/EGL library as DT_NEEDED,
   despite all of them being Wayland clients).
4. **Workspace layouts differ per crate — get the build target wrong and you build the wrong
   thing (or drag in an unrelated member's failing dependency)**: `cosmic-panel` needed
   `cd cosmic-panel-bin && cargo build` (workspace member, no `default-members`, would
   otherwise try to build sibling members too); `cosmic-settings-daemon`/
   `cosmic-notifications`/`cosmic-bg` build from their checkout root (package IS the workspace
   root); `cosmic-settings` has an explicit `default-members = ["cosmic-settings"]` so building
   from root only builds the one binary, skipping its `page-*`/`subscriptions/*` workspace
   siblings. Always check `default-members` and `[[bin]]` placement before invoking `cargo
   build` at a workspace root.
5. **A background build can be killed mid-compile by an unrelated host/session event (agent
   watchdog, not a build failure)** with no `=== rc=` marker and no output binary — this
   happened once to the cosmic-settings aarch64 attempt (background b8pd9jdxv, silently died
   with `target/aarch64-unknown-linux-musl/release/` containing only lock files, zero
   cargo/rustc processes alive, no error in the log). Distinguish this from a real build
   failure by checking for the `=== rc=` trailer build-rust.sh always appends; its absence +
   an incomplete `target/` dir means "re-run", not "diagnose a compile error". Re-running the
   identical invocation (background bhl14nqu2) succeeded cleanly on the first attempt.

## Runtime-closure note (all 8 linked binaries)
Static/link-time `DT_NEEDED` is deliberately small (1–3 entries, always a subset of `{libc.so,
libxkbcommon.so.0, libudev.so.1}`) because every Wayland/EGL/GL library is resolved via
`dlopen` at runtime, not linked. The actual runtime closure for a working desktop session still
needs the full `m3-gl-stack` GL/Wayland stack (`libwayland-{client,server,cursor,egl}`, `libEGL`,
`libGLESv2`, `libgbm`, the gallium megadriver, etc. — see `m3-gl-stack/NOTES.md`'s anvil runtime-
install manifest) dlopen-able on target, plus `libxkbcommon`/`libudev` (from `m4-input-ship`) as
hard links. No new runtime-only gap was found beyond what m3/m4 already stage.

## Reproduce
```
D=~/code/leandros-artifacts/m6-session-bins
sh "$D/gen-cargo-config.sh" "$D/src/<crate>"        # once per crate checkout
sh "$D/build-rust.sh" src/cosmic-session x86_64|aarch64 --no-default-features
sh "$D/build-rust.sh" src/cosmic-panel/cosmic-panel-bin x86_64|aarch64
sh "$D/build-rust.sh" src/cosmic-notifications x86_64|aarch64
sh "$D/build-rust.sh" src/cosmic-bg x86_64|aarch64 --no-default-features
sh "$D/build-rust.sh" src/cosmic-osd x86_64|aarch64
sh "$D/build-rust.sh" src/cosmic-launcher x86_64|aarch64
sh "$D/build-rust.sh" src/cosmic-applibrary x86_64|aarch64
sh "$D/build-rust.sh" src/cosmic-settings x86_64|aarch64 --no-default-features --features a11y,linux,single-instance,wgpu,systemd
sh "$D/verify-elf.sh" <arch> "$D/out/<name>-<arch>"
sh "$D/verify-closure.sh" <arch> "$D/out/<name>-<arch>"
```

## Artifact paths
All binaries: `~/code/leandros-artifacts/m6-session-bins/out/<crate>-<arch>` (16 files: the 8
linked crates above × {x86_64,aarch64}). Build logs: `~/code/leandros-artifacts/m6-session-bins/
logs/<crate>-<arch>.log`. Sources (unmodified + `.cargo/config.toml` only):
`~/code/leandros-artifacts/m6-session-bins/src/<crate>/`.
