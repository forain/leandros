# M5 session-ship manifest (cosmic-comp closure + D-Bus session + fonts + renderer env)

Host-only, repo-read-only lane. No git/QEMU/image writes made. Everything below
lives under `~/code/leandros-artifacts/m5-session-ship/` and
`~/code/leandros-artifacts/notes/`.

## 1. cosmic-comp closure verify — CLOSED, both arches, zero gaps

Verifier: `m5-session-ship/verify-closure.sh <arch> <elf>` (same shape as
`m4-input-ship/verify-closure.sh`, repointed at the union of
`m3-gl-stack/sysroot-<arch>/usr/lib` + `m4-input-ship/<arch>/usr/lib`, M4
taking precedence).

Ran against `m3-gl-stack/out/cosmic-comp-{x86_64,aarch64}` (already fully
linked by the M3 lane). Both arches: **ET_DYN, INTERP=/lib/ld-musl-<arch>.so.1,
DT_NEEDED closure fully resolves, zero MISSING entries.**

Direct NEEDED (8, identical both arches):
```
libdisplay-info.so.3   [m4-input-ship]      -> libdisplay-info.so.0.3.0
libgbm.so.1            [m3-gl-stack sysroot]-> libgbm.so.1.0.0
libseat.so.1           [m4-input-ship]      -> libseat.so.1.0.0
libudev.so.1           [m4-input-ship]      -> libudev.so.1.0.0
libinput.so.10         [m4-input-ship]      -> libinput.so.10.13.0
libpixman-1.so.0       [m4-input-ship]      -> libpixman-1.so.0.44.2
libxkbcommon.so.0      [m4-input-ship]      -> libxkbcommon.so.0.8.0
libc.so                [m3-gl-stack sysroot]
```
Transitively pulled in (via libinput): `libmtdev.so.1`, `libevdev.so.2`
(both m4-input-ship). `libdrm.so.2` also resolves (m3-gl-stack sysroot, pulled
via libgbm). No host libs anywhere in the chain. This matches M3's NOTES.md
claim exactly (NEEDED=8) — now independently re-verified from this lane
against the actual staged M4 set, not just the raw sysroot.

Runtime dlopen (not DT_NEEDED, per M3/M4 notes, unchanged): backend_egl pulls
the whole GL stack (`libEGL.so.1` -> `libgallium-25.3.6.so`, `libexpat.so.1`,
`libz.so.1`, `libwayland-{client,server}.so.0`, `libffi.so.8`, `libGLESv2.so.2`,
`/usr/lib/gbm/dri_gbm.so`) — all already in `m3-gl-stack/sysroot-<arch>`.

**No gaps to report** — cosmic-comp's full closure is already covered by the
existing M3 ∪ M4 ship sets. Nothing new needs to be built for linking; M5 only
adds runtime-behavior pieces (session bus, fonts) that aren't ELF dependencies.

## 2. Session bus staging

Staged into `m5-session-ship/<arch>/` with the **on-image paths already
hardcoded into the launcher** (`ports/dbus/session-pkg/dbus-run-session`
defaults: `BUSD_BIN=/usr/libexec/busd`, `SESSION_CONF=/usr/share/dbus-1/session.conf`):

| staged file | on-image path | arch |
|---|---|---|
| `m5-session-ship/x86_64/usr/libexec/busd` | `/usr/libexec/busd` | x86_64 |
| `m5-session-ship/aarch64/usr/libexec/busd` | `/usr/libexec/busd` | aarch64 |
| `m5-session-ship/<arch>/usr/share/dbus-1/session.conf` | `/usr/share/dbus-1/session.conf` | both (identical file) |
| `m5-session-ship/<arch>/usr/bin/dbus-run-session` | `/usr/bin/dbus-run-session` | both (identical file) |

Source: busd binaries built earlier this effort at
`~/.claude-forain/jobs/afde2e74/tmp/s5-busd-probe/busd/target/{x86_64,aarch64}-unknown-linux-musl/release/busd`
(referenced from `ports/dbus/RUNTIME-NOTES.md`; not in the repo, a job tmp dir
— read-only source, copied not moved). `session.conf` and `dbus-run-session`
copied verbatim from `ports/dbus/session-pkg/` (repo, read-only source).

**Arch/type verification (this lane, fresh):**
```
busd (x86_64):  ELF 64-bit LSB executable, x86-64,   statically linked, stripped
busd (aarch64): ELF 64-bit LSB executable, AArch64,  statically linked, stripped
llvm-readelf -h: Type: EXEC (Executable file)   [confirms ET_EXEC, not ET_DYN — matches "built static" claim]
```
Both binaries are the correct arch and genuinely static ET_EXEC — no
PT_INTERP, no DT_NEEDED possible to check (statically linked), nothing to
resolve against the M3/M4 sysroots. `busd --version` reports 0.5.0 per prior
container-tested run (RUNTIME-NOTES.md task 1).

**Launcher shell-feature check** (`dbus-run-session`, POSIX sh):
`sh -n` clean (re-verified this lane). Features used: `set -u`, functions,
`trap ... EXIT INT TERM`, `$(( ))` arithmetic, `[ -s file ]` / `[ -x ]` tests,
`kill -0`, numbered-fd redirection (`3>"$READY_FILE"`), `"$@"`, `$!`, `$?`.
No `mkfifo`/FIFOs, no `sleep`, no bashisms (no arrays, `[[`, `local`,
process substitution, here-strings) — confirmed against
`ports/dbus/RUNTIME-NOTES.md`'s own rationale (busy-poll-on-a-regular-file
workaround, written specifically because LeandrOS has no working FIFO
rendezvous and no `sleep`). Already container-verified end-to-end under
busybox ash (a stricter POSIX sh than brush); this lane did not re-run it
live (no QEMU access) but did re-confirm syntax + the on-disk env-var default
paths line up with where these files are staged.

## 3. Font audit

**Source of truth:** `../cosmic-epoch/cosmic-comp/Cargo.toml` (direct dep,
`cosmic-text = { version = "0.19", features = ["shape-run-cache"] }` — no
`default-features = false`, so cosmic-text's own defaults apply) +
`cosmic-text-0.19.0` (registry cache) + `fontdb-0.23.0` (registry cache) +
the vendored `libcosmic` git checkout at
`~/.cargo/git/checkouts/libcosmic-41009aea1d72760b/d922ac3/`
(pop-os/libcosmic, pulled in by cosmic-comp's own `libcosmic`/`iced_tiny_skia`/
`iced_graphics` deps). cosmic-comp renders its own text directly (window
titles / OSD via cosmic-text + iced_tiny_skia), confirmed — this is not a
theoretical dependency, `cosmic-text` is in cosmic-comp's own `[dependencies]`.

**fontconfig feature is ON, not off:** `cosmic-text-0.19.0/Cargo.toml` has
`default = ["std", "swash", "fontconfig"]` and none of cosmic-comp / iced /
iced_tiny_skia / iced_graphics disable cosmic-text's default features, so
fontdb's `fontconfig` feature (pure-Rust `fontconfig-parser` crate, NOT a
libfontconfig.so link — confirmed no `fontconfig` shared lib anywhere in the
dependency graph) is active. This is real — `fontconfig-parser` shows up as
a resolved fontdb dependency in cosmic-comp's own `Cargo.lock`.

**Scan-dir behavior on Linux, exact code path** (`fontdb-0.23.0/src/lib.rs`,
`load_system_fonts()`):
1. `#[cfg(feature = "fontconfig")]` → tries `load_fontconfig()` first: parses
   `$FONTCONFIG_FILE`, else `$XDG_CONFIG_HOME/fontconfig/fonts.conf` (or
   `~/.config/fontconfig/fonts.conf`), else `/etc/fonts/local.conf` +
   `/etc/fonts/fonts.conf`, via the pure-Rust `fontconfig_parser` crate (no
   libfontconfig.so needed). **If none of those config files exist** (true on
   LeandrOS — no fontconfig package), `fontconfig.dirs` stays empty and
   `load_fontconfig()` explicitly `return false`s (line ~561: `if
   fontconfig.dirs.is_empty() { return false; }`).
2. On that `false`, `load_system_fonts()` logs
   `"Fallback to loading from known font dir paths."` and calls
   `load_no_fontconfig()`, which scans, **in order**:
   - `/usr/share/fonts/`
   - `/usr/local/share/fonts/`
   - `$HOME/.fonts`
   - `$HOME/.local/share/fonts`

   **→ This is the exact on-image directory LeandrOS needs populated:
   `/usr/share/fonts/` (root-only bring-up, no `$HOME` reliably set yet)
   is the one that matters.**

**Zero-fonts-found behavior: soft-fail, NOT a panic.** Grepped
`cosmic-text-0.19.0/src/font/system.rs` (the whole `FontSystem`/font-matching
path) for `panic!`/`.unwrap()`/`.expect(` — none found tied to "zero faces in
db". Font matching (`get_font_matches`) builds a `Vec` of match keys and
falls through generic-family aliases and a fallback list; with zero fonts
loaded the match list is simply empty and `log::error!` fires ("Could not get
face from db, that should've been there.") on the one place that assumes a
face exists (line ~405) for a specific inner case, but the overall function
still returns (an `Arc::new(font_match_keys)`, possibly empty) — no abort.
**Conclusion: with zero fonts on disk, cosmic-comp will start and run fine;
any text it draws (window titles, whatever OSD it renders) will simply be
invisible/blank, not a crash.** This de-risks M6 bring-up order (fonts are a
visual-quality issue, not a boot blocker) but they're still needed for a
usable desktop.

**Default font family cosmic actually expects — NOT Fira Sans/Inter (prior
memory guess was wrong for this checkout), verified from source:**
- `libcosmic/src/config/mod.rs`: `SANS_FAMILY_DEFAULT = "Open Sans"` (used
  for `interface_font()`, i.e. all normal UI text) and
  `MONO_FAMILY_DEFAULT = "Noto Sans Mono"` (used for `mono()`/monospace text).
- Independently, `cosmic-text`'s own `FontSystem::new_with_fonts()` sets the
  fontdb generic-alias defaults: `set_sans_serif_family("Open Sans")`,
  `set_monospace_family("Noto Sans Mono")`, `set_serif_family("DejaVu Serif")`
  — same two family names show up from both angles, so this is the real,
  converged answer, not a coincidence.
- The exact files are already vendored in the libcosmic checkout itself at
  `res/open-sans/*.ttf` and `res/noto/NotoSansMono-*.ttf` — these are the
  literal files upstream ships/embeds for its own default look, so staging
  them here is staging exactly what cosmic expects, not a substitute.

**Fonts staged** → `m5-session-ship/share/fonts/` (install to
`/usr/share/fonts/` on-image, per the scan-dir finding above):
```
share/fonts/open-sans/OpenSans-Regular.ttf    (interface_font default weight)
share/fonts/open-sans/OpenSans-Bold.ttf       (font::bold())
share/fonts/open-sans/OpenSans-Semibold.ttf   (font::semibold(), used for headers/titles)
share/fonts/open-sans/LICENSE                 (SIL OFL 1.1)
share/fonts/noto/NotoSansMono-Regular.ttf     (monospace_font default weight)
share/fonts/noto/NotoSansMono-Bold.ttf
share/fonts/noto/LICENSE                      (SIL OFL 1.1)
```
Total ~1.6 MB (5 font files + 2 license files). Deliberately dropped
`OpenSans-Light`/`OpenSans-ExtraBold` (libcosmic exposes `font::light()` too,
but Regular/Bold/Semibold cover default+emphasis+heading; add Light later if
a specific widget turns out to need it — small enough to add without
re-architecting anything). Both fonts SIL OFL 1.1 (license text copied
alongside, source: upstream Open Sans / Noto Sans Mono projects via the
libcosmic repo's own `res/*/LICENSE` files) — redistribution is explicitly
permitted.
**Files came from this Mac's already-cloned `libcosmic` git checkout**
(`~/.cargo/git/checkouts/libcosmic-41009aea1d72760b/d922ac3/res/`), not a
fresh download — these are the literal upstream-shipped files, so no
version/hinting mismatch risk versus a substitute font.

## 4. ICED_BACKEND / renderer env var — VERIFIED (memory's guess was correct)

Source: `~/.cargo/git/checkouts/libcosmic-41009aea1d72760b/d922ac3/iced/renderer/src/fallback.rs`:
```rust
let backends = backend
    .map(str::to_owned)
    .or_else(|| env::var("ICED_BACKEND").ok());
```
and the tiny-skia backend's own matcher
(`iced/tiny_skia/src/lib.rs:446`, `iced/tiny_skia/src/window/compositor.rs:39`):
```rust
None | Some("tiny-skia") | Some("tiny_skia") => { ... }   // accepted
!["tiny-skia", "tiny_skia"].contains(&backend) => reject   // anything else
```
**Verified name/value: `ICED_BACKEND=tiny-skia`** (`tiny_skia` with an
underscore also accepted, per the match arm, but `tiny-skia` with a hyphen is
the canonical form used everywhere else in the fork, e.g. the compositor's
self-reported `backend: "tiny-skia"` string). Comma-separated fallback lists
are supported (`ICED_BACKEND=tiny-skia,wgpu` tries tiny-skia first, wgpu
second) — a single value is enough for our case since we want to force
software-only.

Other env vars found in the same fork, for completeness (not the ask, but
adjacent and worth recording for M6):
- `ICED_THEME` (`iced/core/src/theme.rs`) — light/dark theme override.
- `ICED_PRESENT_MODE` (`iced/wgpu/src/settings.rs`) — wgpu-only, irrelevant if
  forcing tiny-skia.
- `WGPU_POWER_PREF`, `WGPU_ADAPTER_NAME` (`iced/wgpu/src/window/compositor.rs`)
  — wgpu-only, irrelevant under tiny-skia.
- **`COSMIC_BACKEND`** (this one is in `cosmic-comp` itself, not iced/libcosmic
  — `cosmic-comp/src/backend/mod.rs:25`, `init_backend_auto()`): selects
  cosmic-comp's own compositor backend, values `"x11"` / `"winit"` / `"kms"`.
  With neither `$DISPLAY` nor `$WAYLAND_DISPLAY` set (true for a bare-metal/
  QEMU LeandrOS boot with no host compositor), the auto-detect already falls
  through to `kms::init_backend` by default — but **setting
  `COSMIC_BACKEND=kms` explicitly for M6 bring-up removes the auto-detect
  branch entirely** and is the safer choice for a scripted boot.
- `COSMIC_SCALE` (`libcosmic/src/app/settings.rs`) — UI scale factor override,
  relevant for libcosmic apps (panel/applets), not cosmic-comp itself.
- `COSMIC_SINGLE_INSTANCE`, `COSMIC_PANEL_*` — libcosmic-app-level, not
  renderer-related.

**For M6:** `ICED_BACKEND=tiny-skia COSMIC_BACKEND=kms` is the env pair to set
before launching cosmic-comp / any libcosmic-based applet on LeandrOS.

## Proposed mkfs-f2fs-populated.py additions (snippet, NOT applied to the repo)

```python
# --- M5: D-Bus session bus + cosmic-comp font set ---
# Source dir: ~/code/leandros-artifacts/m5-session-ship/<arch>/ and .../share/
add_file(image, host=f"{M5}/{arch}/usr/libexec/busd",
         target="/usr/libexec/busd", mode=0o755)
add_file(image, host=f"{M5}/{arch}/usr/share/dbus-1/session.conf",
         target="/usr/share/dbus-1/session.conf", mode=0o644)
add_file(image, host=f"{M5}/{arch}/usr/bin/dbus-run-session",
         target="/usr/bin/dbus-run-session", mode=0o755)

# Fonts (arch-independent, same files both images)
for f in ["open-sans/OpenSans-Regular.ttf", "open-sans/OpenSans-Bold.ttf",
          "open-sans/OpenSans-Semibold.ttf", "open-sans/LICENSE",
          "noto/NotoSansMono-Regular.ttf", "noto/NotoSansMono-Bold.ttf",
          "noto/LICENSE"]:
    add_file(image, host=f"{M5}/share/fonts/{f}",
             target=f"/usr/share/fonts/{f}", mode=0o644)

# XDG_RUNTIME_DIR for the session bus socket (dbus-run-session defaults to
# /run/user/0 unless $XDG_RUNTIME_DIR is set) — ensure the dir exists on-image
add_dir(image, target="/run/user/0", mode=0o700)

# M6 env vars to export from whatever launches cosmic-comp (init/getty/a
# start-cosmic-equivalent script), NOT baked into the image as files:
#   ICED_BACKEND=tiny-skia
#   COSMIC_BACKEND=kms
```

## Files
- Checkpoint: `~/code/leandros-artifacts/notes/m5-prep-progress.md`
- This manifest: `~/code/leandros-artifacts/notes/m5-session-manifest.md`
- Closure verifier: `~/code/leandros-artifacts/m5-session-ship/verify-closure.sh`
- Staged tree: `~/code/leandros-artifacts/m5-session-ship/{x86_64,aarch64}/usr/{libexec,bin,share/dbus-1}/`, `~/code/leandros-artifacts/m5-session-ship/share/fonts/{open-sans,noto}/`
