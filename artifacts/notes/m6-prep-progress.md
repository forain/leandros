# M6 session-bins prep — checkpoint

Lane: HOST-ONLY, repo-read-only. Workdir: ~/code/leandros-artifacts/m6-session-bins/
Foundation: m3-gl-stack/sysroot-{x86_64,aarch64} (read-only ref) + own toolchain copy
(m6-session-bins/toolchain, copied from m3-gl-stack/toolchain) + own .cargo/config.toml
per crate (gen-cargo-config.sh) pointing zig-ld-lld at the m3 sysroots.
Sources: /Users/forain/code/cosmic-epoch/<crate> copied into m6-session-bins/src/<crate>
(read-only checkout upstream; our copy gets .cargo/config.toml added, no other patches
unless documented).

## STATUS (updates after every attempt)
- Setup: toolchain copied, build-rust.sh/verify-elf.sh/verify-closure.sh/gen-cargo-config.sh written. DONE.
- cosmic-session x86_64: LINKED clean. `cargo +nightly build --release --target x86_64-unknown-linux-musl --no-default-features`
  (drops systemd/logind-zbus/tracing-journald default features). ET_DYN, INTERP=/lib/ld-musl-x86_64.so.1,
  NEEDED=1 (libc.so only — pure-Rust deps, zbus is pure-Rust D-Bus, no libdbus link). closure CLOSED.
  out/cosmic-session-x86_64.
- cosmic-session aarch64: LINKED clean, same as x86_64 (ET_DYN, NEEDED=1 libc.so, closure CLOSED).
  out/cosmic-session-aarch64.
- cosmic-settings-daemon x86_64: BLOCKED. `cargo build --release --target x86_64-unknown-linux-musl`
  (workspace root = the daemon package itself) fails at build-script stage: audio-server (a direct
  path dep of the main daemon, NOT an unrelated workspace member) depends on `cosmic-pipewire`, whose
  `pipewire-sys`/`libspa-sys` build.rs pkg-config-probes `libpipewire-0.3` — not staged in
  m3-gl-stack/m4-input-ship/m5-session-ship, not present on host either. NO feature flag exists in
  cosmic-settings-daemon's or audio-server's Cargo.toml to gate this off (unconditional path dep) —
  so this is not a "drop a default feature" fix, it is a genuine missing-library closure gap.
  Fixing it would mean cross-building pipewire+libspa (new port, out of this lane's scope: "cross-build
  against the EXISTING foundation"). NOT ATTEMPTING aarch64 (identical dep graph, same failure expected).
  NEW LANDMINE recorded in manifest.
- cosmic-panel x86_64: LINKED clean (default features; no source patch). Built from
  src/cosmic-panel/cosmic-panel-bin (workspace member, [[bin]] name=cosmic-panel; cargo
  auto-discovers the workspace-root .cargo/config.toml by walking up). ET_DYN,
  INTERP=/lib/ld-musl-x86_64.so.1, NEEDED=2 (libxkbcommon.so.0 [m4-input-ship], libc.so
  [m3-gl-stack sysroot]) — closure CLOSED. Despite smithay `use_system_lib` +
  wayland-backend `client_system` feature, NO libwayland-{client,server,egl} or libEGL
  DT_NEEDED — confirmed via `strings`: wayland-sys/smithay still dlopen them at runtime
  ("Library libwayland-client.so could not be loaded" fallback strings present, "Failed to
  load LibEGL" too). Same dlopen-not-link pattern as anvil/cosmic-comp in m3-gl-stack.
  Runtime closure needs libwayland-{client,server}.so + libwayland-egl.so.1 + libEGL.so.1
  (all present in m3-gl-stack sysroot) dlopen-able at runtime even though not DT_NEEDED.
  out/cosmic-panel-x86_64. aarch64: IN PROGRESS (background bhxxxb2wf).
- cosmic-notifications x86_64: LINKED clean (default features incl. `systemd` -> pulls
  tracing-journald only, no libsystemd C lib). libcosmic winit+wayland+multi-window+a11y+
  dbus-config features all resolved without adding any new DT_NEEDED beyond cosmic-panel's
  set. ET_DYN, INTERP=/lib/ld-musl-x86_64.so.1, NEEDED=2 (libxkbcommon.so.0 [m4-input-ship],
  libc.so [m3-gl-stack sysroot]) — closure CLOSED. Same dlopen-not-link wayland/EGL pattern.
  out/cosmic-notifications-x86_64. aarch64: LINKED clean (background bczizrqs2 confirmed rc=0),
  same NEEDED set as x86_64. closure CLOSED. out/cosmic-notifications-aarch64.
- cosmic-panel aarch64: LINKED clean (background bhxxxb2wf confirmed rc=0). Same NEEDED set as
  x86_64 (libxkbcommon.so.0, libc.so). closure CLOSED. out/cosmic-panel-aarch64.
- CORE SET (1,3,4) COMPLETE both arches. #2 (cosmic-settings-daemon) remains BLOCKED (pipewire gap).
- stretch: sources staged for cosmic-bg, cosmic-launcher, cosmic-applibrary, cosmic-settings,
  cosmic-osd (rsync + gen-cargo-config.sh done for all 5).
  - cosmic-bg x86_64: default-features build FAILED — `avif` default feature pulls
    `image/avif-native` -> `dav1d-sys` -> pkg-config probe for `libdav1d`, NOT staged anywhere
    (m3/m4/m5/host all lack it). Retried with `--no-default-features` (drops `avif`; that's
    cosmic-bg's ONLY feature) -> LINKED clean. ET_DYN, INTERP=/lib/ld-musl-x86_64.so.1,
    NEEDED=1 (libc.so only) — closure CLOSED. out/cosmic-bg-x86_64.
    NEW LANDMINE: libdav1d/AV1 not staged (same shape as the pipewire gap: a real missing
    system lib, not a source-patch problem; --no-default-features is the correct fix here
    since avif support is optional wallpaper-decoding, unlike settings-daemon's pipewire).
  - cosmic-osd x86_64: LINKED clean, default features. ET_DYN, INTERP=/lib/ld-musl-x86_64.so.1,
    NEEDED=2 (libxkbcommon.so.0 [m4-input-ship], libc.so [m3-gl-stack sysroot]) — closure
    CLOSED. out/cosmic-osd-x86_64. logind-zbus dep is pure-Rust D-Bus, no libsystemd link.
  - cosmic-launcher x86_64: LINKED clean, default features (`default = []`, no gates needed).
    ET_DYN, INTERP=/lib/ld-musl-x86_64.so.1, NEEDED=2 (libxkbcommon.so.0, libc.so) — closure
    CLOSED. out/cosmic-launcher-x86_64.
  - cosmic-applibrary x86_64: LINKED clean, default features incl. `wgpu` (background bp3t6nckf,
    rc=0). wgpu feature did NOT add any new DT_NEEDED (presumably Vulkan/Metal loader also
    dlopen-based, or the wgpu backend falls back cleanly at build time). ET_DYN,
    INTERP=/lib/ld-musl-x86_64.so.1, NEEDED=2 (libxkbcommon.so.0, libc.so) — closure CLOSED.
    out/cosmic-applibrary-x86_64.
  - cosmic-settings x86_64: default-features build FAILED (background b6mpkjdqr, rc=101) — SAME
    dav1d-sys/libpipewire-shape gap: cosmic-settings' own `default = ["a11y","avif","linux",
    "single-instance","wgpu","systemd"]` includes `avif` (`avif = ["image/avif-native"]`),
    pulling `dav1d-sys` -> pkg-config probe for `libdav1d`, not staged anywhere. Retried with
    `--no-default-features --features a11y,linux,single-instance,wgpu,systemd` (keeps every
    default EXCEPT avif) -> LINKED clean (background btuyuux9i, rc=0). CONFIRMED: `page-sound`
    -> `cosmic-settings-audio-client` is indeed a pure-Rust IPC client — no libpipewire
    dependency at all in this binary. ET_DYN, INTERP=/lib/ld-musl-x86_64.so.1, NEEDED=3
    (libudev.so.1 [m4-input-ship, new vs. the other stretch bins — likely page-networking/
    page-bluetooth], libxkbcommon.so.0 [m4-input-ship], libc.so [m3-gl-stack sysroot]) —
    closure CLOSED. out/cosmic-settings-x86_64.
    aarch64 (same flags): IN PROGRESS (background b8pd9jdxv).
  - cosmic-bg aarch64: LINKED clean (background birk5frg2, rc=0), same as x86_64: ET_DYN,
    INTERP=/lib/ld-musl-aarch64.so.1, NEEDED=1 (libc.so) — closure CLOSED. out/cosmic-bg-aarch64.
  - cosmic-osd aarch64: LINKED clean (background b43fo3ogl, rc=0). Same NEEDED as x86_64
    (libxkbcommon.so.0, libc.so). closure CLOSED. out/cosmic-osd-aarch64.
  - cosmic-launcher aarch64: LINKED clean (background ba74shwi6, rc=0). Same NEEDED as x86_64
    (libxkbcommon.so.0, libc.so). closure CLOSED. out/cosmic-launcher-aarch64.
  - cosmic-applibrary aarch64: LINKED clean (background bg1obh410, rc=0). Same NEEDED as x86_64
    (libxkbcommon.so.0, libc.so). closure CLOSED. out/cosmic-applibrary-aarch64.
  - cosmic-settings x86_64 retry (--no-default-features --features a11y,linux,single-instance,
    wgpu,systemd): LINKED clean (background btuyuux9i, rc=0). ET_DYN,
    INTERP=/lib/ld-musl-x86_64.so.1, NEEDED=3 (libudev.so.1 [m4-input-ship, new vs. the other
    stretch bins], libxkbcommon.so.0 [m4-input-ship], libc.so [m3-gl-stack sysroot]) — closure
    CLOSED. out/cosmic-settings-x86_64. CONFIRMED page-sound's audio-client is pure-Rust IPC —
    no libpipewire needed by this binary.
  - cosmic-settings aarch64 (same flags): first attempt (background b8pd9jdxv) DIED MID-BUILD —
    no `=== rc=` marker in log, no binary in target/, zero cargo/rustc processes alive when
    checked (killed by an agent-session watchdog unrelated to the build itself, not a build
    failure). RELAUNCHED as background bhl14nqu2 (same invocation, itself the tracked
    run_in_background command) — LINKED clean (rc=0). ET_DYN, INTERP=/lib/ld-musl-aarch64.so.1,
    NEEDED=3 (libudev.so.1, libxkbcommon.so.0, libc.so) — closure CLOSED.
    out/cosmic-settings-aarch64.
  - cosmic-settings is a workspace with `default-members = ["cosmic-settings"]` (nested dir) —
    building from the checkout root builds just the cosmic-settings binary by default, no -p needed.

## LANE COMPLETE
All 9 target binaries attempted (4 core priority + 5 stretch). 8 LINKED clean on BOTH arches
(cosmic-session, cosmic-panel, cosmic-notifications, cosmic-bg, cosmic-osd, cosmic-launcher,
cosmic-applibrary, cosmic-settings). 1 BLOCKED (cosmic-settings-daemon, libpipewire-0.3 gap, no
feature escape hatch, aarch64 not attempted — see manifest). All 16 produced binaries batch-
verified in one final pass: ET_DYN, correct PT_INTERP, correct e_machine, closure CLOSED against
m5-session-ship ∪ m4-input-ship ∪ m3-gl-stack/sysroot-<arch>. Final writeup in
notes/m6-bins-manifest.md. No live background children remain — nothing further to join on.

## Discipline note (post-incident)
Earlier in this lane a detached "waiter" shell (bde4cmhn0, an until-loop polling two log files)
was spawned WHILE two builds were already running as their own run_in_background tasks
(b12q7wewz, beiaczkax) — redundant, and several turns were then burned manually polling logs
instead of trusting the harness. Corrected going forward: every `cargo build` invocation IS the
run_in_background command; no separate waiter/watcher shells. (In the end all three — the two
builds and the waiter — did complete and notify correctly; the risk flagged by the coordinator
was the polling behavior between them, not a genuinely stuck process.)
