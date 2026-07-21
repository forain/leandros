# D3 Input-Stack — progress log & final report

Workdir: `/Users/forain/.claude-forain/jobs/afde2e74/tmp/d3-input-stack`
Lane: D3 (seat/udev/input userland), host-only. Pull forward libseat + libudev
ABI shims and cross-build libxkbcommon, pixman, libdisplay-info + XKB data for
`x86_64-linux-musl` and `aarch64-linux-musl`. Reuses the S3-proven zig-cc + meson
cross toolchain.

## VERDICT: ALL GREEN, both arches.

| Component | Kind | x86_64 | aarch64 | Notes |
|---|---|---|---|---|
| libseat shim | write (197 LoC C) | BUILD OK, link OK | BUILD OK, link OK | soname libseat.so.1; 11/11 symbols; 10/10 libseat-sys imports covered |
| libudev shim | write (877 LoC C) | BUILD OK, link OK | BUILD OK, link OK | soname libudev.so.1; 92 exports; 82/82 libudev-sys imports covered |
| pixman 0.44.2 | meson cross | BUILD OK, link OK | BUILD OK, link OK | soname libpixman-1.so.0 |
| libxkbcommon 1.8.0 | meson cross | BUILD OK, link OK | BUILD OK, link OK | soname libxkbcommon.so.0; XKB root `/usr/share/X11/xkb`, rules `evdev` |
| libdisplay-info 0.3.0 | meson cross | BUILD OK, link OK | BUILD OK, link OK | soname libdisplay-info.so.3 |
| xkeyboard-config 2.44 | meson native (data) | staged (arch-independent) | (same data) | ship-set `/usr/share/X11/xkb`, 3.8 MB |

Shim link-time smoke tests: **4/4 PASS** (2 shims × 2 arches). Missing-symbol
audit vs FFI import lists: **0 missing** on both shims.
Meson-lib link-sanity: **6/6 PASS** (3 libs × 2 arches).
Every shim NEEDs only musl `libc.so` (matches D1's dynamic-musl world).

## Layout (organized sysroot + ship-set)

```
sysroot/<arch>/usr/
  include/   libseat.h libudev.h  pixman-1/  xkbcommon/  libdisplay-info/
  lib/       lib{seat,udev}.so{,.1,.1.0.0}
             libpixman-1.so{,.0,.0.44.2}
             libxkbcommon.so{,.0,.0.8.0}
             libdisplay-info.so{,.3,.0.3.0}
             pkgconfig/{libseat,libudev,pixman-1,xkbcommon,libdisplay-info}.pc
  bin/       di-edid-decode         (tool that came with libdisplay-info; harmless)
ship-set/usr/share/X11/xkb/          (compat geometry keycodes rules symbols types)
shims/libseat/  {libseat.c, libseat.h, libseat.map}     <- ready to move into leandros
shims/libudev/  {libudev.c, libudev.h, libudev.map}     <- ready to move into leandros
toolchain/      {x86_64,aarch64}-linux-musl-{cc,c++,ar,ranlib}
cross-musl-{x86_64,aarch64}.ini
build.sh         (meson-lib driver)   build-shims.sh (shim driver)
consumers/       auto-generated link-time consumers + import lists
logs/            per-component build logs
```
`<arch>` ∈ {x86_64, aarch64}; the aarch64 sysroot mirrors x86_64 exactly.

## Shim design (summary; full rationale in the source headers)

**libseat** — "builtin, always-root" backend. Single seat `seat0`, always active,
no VT. `open_seat` calls `enable_seat` immediately (D3 contract) and returns a
pollable `eventfd` (via `get_fd`) that never fires; `dispatch` always reports 0
messages. `open_device` = `open(path, O_RDWR|O_NONBLOCK|O_CLOEXEC)` returning the
fd as both device-id and fd; `close_device` = `close`. `switch_session`/
`disable_seat` are success no-ops.

**libudev** — data-driven static device model (no udevd, no netlink). Table:
card0 + renderD128 (subsystem drm) and event0/1/2 (subsystem input;
keyboard / mouse / touchscreen, each `ID_INPUT=1` + its type property). Syspaths
follow the future synthetic-sysfs contract `/sys/class/{drm,input}/<name>`;
parent input<N>/platform-gpu nodes are modeled for `get_parent*`. Enumerate
honors subsystem match/nomatch + sysname + property filters; other filters are
accepted-but-inert (documented). Monitor = one end of a socketpair that never
delivers; hwdb empty; queue always empty. `udev_util_encode_string` is a faithful
port. When the kernel gains real synthetic sysfs, replace the static table with a
scan of `/sys/class/{drm,input}`; all getters already key off these paths.

## Symbol-export lists vs consumer imports (how derived)

Import lists were derived from the exact crate/lib versions pinned in
`cosmic-comp/Cargo.lock`, cross-checked against upstream ABI headers:

- **libseat** ABI header: seatd master `include/libseat.h` (11 functions).
  Import set: `libseat-sys 0.2.0` `extern "C"` block = **10** functions
  (all except `libseat_set_log_handler`, which the shim still exports for ABI
  completeness). smithay's `libseat 0.2.4` uses this via libseat-sys. Result:
  shim exports 11, covers 10/10 imports. `consumers/libseat_imports.txt`.
- **libudev** ABI header: systemd main `src/libudev/libudev.h`.
  Import set = union of:
  - `libudev-sys 0.1.4` `extern "C"` block = **82** functions (the ABI the
    `udev 0.9.3` crate / smithay bind against),
  - `libinput 1.27.1` real `udev_*` calls (grepped `src/`; a strict subset of
    the 82 — the `udev_input_*`/`udev_seat_*` names in libinput are its own
    internal symbols, not libudev API),
  - Mesa `udev_*` calls (only `src/vulkan/wsi/wsi_common_display.c`, which is
    NOT in our softpipe/EGL build path — still satisfied).
  - libdrm was checked: it does **not** call libudev (only a false-match local
    `udev_count`); it reads sysfs directly → Mesa's GBM/EGL DRM path needs the
    synthetic sysfs (K4), not this shim.
  Result: shim exports 92, covers 82/82 imports (missing = 0).
  `consumers/libudev_imports.txt`.

Verification method: generated a C consumer that takes `&sym` of every imported
name (declarations from the ABI header) and dynamically linked it against the
shim `.so`. A missing export would fail the link; all links succeed and the
resulting binary records `DT_NEEDED [libudev.so.1] / [libseat.so.1]` (our
sonames). Cross-checked with `llvm-nm -D --defined-only` diff → 0 missing.

## Reproduce

```
./build.sh <pixman|libdisplay-info|libxkbcommon> <x86_64|aarch64>   # meson libs
./build-shims.sh                                                    # both shims, both arches
# XKB data (native, arch-independent):
meson setup build/xkeyboard-config src/xkeyboard-config-* --prefix=/usr \
  -Dxkb-base=/usr/share/X11/xkb -Dnls=false && \
DESTDIR=$PWD/ship-set ninja -C build/xkeyboard-config install
```
Env needed (S3-proven): `PATH=/opt/homebrew/opt/bison/bin:$PATH` (Apple bison 2.3
is too old for libxkbcommon's parser); homebrew python3 for meson. ELF inspection:
`/opt/homebrew/opt/llvm/bin/llvm-{nm,readelf}` (zig has no nm/readelf subcommands).

## Blocker classification (S3 style)

1. **libdisplay-info hwdata dependency — HOST, TRIVIAL (resolved).**
   `meson.build` requires `hwdata`'s `pnp.ids` at build time (codegen). macOS has
   no `hwdata`; meson's native `dependency('hwdata', native:true)` did not pick up
   a hand-written `hwdata.pc` via `PKG_CONFIG_PATH` (meson doesn't propagate it to
   the native probe). Fix: fetched real `pnp.ids` from `vcrhonek/hwdata` and
   pointed the vendored `meson.build` fallback at it (one-line edit, documented).
   Prod fix: `brew install hwdata` or ship pnp.ids at `/usr/share/hwdata/`.
2. **libdisplay-info `-Dtests=` option — HOST, TRIVIAL (resolved).**
   0.3.0 has NO meson options; test/tool subdirs are unconditional (they compile
   under cross but are never run — `needs_exe_wrapper=true`). Dropped the flag.
3. **zig-cc `--version-script <path>` normalization — recurs from S3 (already
   handled).** The toolchain `cc`/`c++` wrappers merge `-Wl,--version-script <file>`
   into `=`-form so lld doesn't misclassify the positional `.map`/`.sym` path.
   Used by both shims and by the meson libs' internal version scripts.
4. **Apple bison 2.3 too old for libxkbcommon — HOST (S3-known).** Needs bison ≥3;
   `/opt/homebrew/opt/bison/bin` ahead on PATH.
No musl-vs-glibc source issues in any of the five C components.

## Handoff / integration notes (for moving into the leandros repo)

- Shim sources are self-contained C11 + POSIX + `sys/eventfd.h`/`socketpair`; drop
  `shims/libseat` and `shims/libudev` into the repo (e.g. `ports/` or a
  `shims/` tree) and build with the same soname + version-script recipe.
- The shims currently model a FIXED device set. Wire them to the kernel's
  synthetic sysfs (K4) later by replacing the static table with a `/sys/class`
  scan; the syspath contract already matches.
- `/dev/input/event2` is modeled as `ID_INPUT_TOUCHSCREEN` (absolute). Per plan
  D3 the virtio-tablet is an absolute pointer; if libinput should treat it as an
  absolute-pointer rather than touchscreen, flip the property in `props_touch`
  (data-driven, one edit) once the evdev node's semantics are finalized in K4.
- libxkbcommon expects XKB data at `/usr/share/X11/xkb` (baked-in
  `DFLT_XKB_CONFIG_ROOT`, default ruleset `evdev`). Install the `ship-set` tree
  into the f2fs image. All five `.so`s NEED musl `libc.so` → they ride ld-musl
  (D1), consistent with the Mesa ship-set.
```
