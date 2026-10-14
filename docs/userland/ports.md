# Ports

`ports/` holds the build recipes for third-party software, each with the exact toolchain
invocation that produces a working binary for LeandrOS, and the notes that explain every
deviation from upstream. This page indexes every port, summarises the design choices each
one records, and covers the larger applications — Firefox, brush, DOOM, MAME and disks-rs —
some of which are built from sibling checkouts rather than from `ports/`.

## How ports are built

Three shapes recur, and each port's recipe says which it uses:

| Shape | Used for | How |
|---|---|---|
| **Cross-built from source, static musl** | busd, greetd, `leandros-sysbus`, `mkfs.fat`, `mkfs.xfs`, brush, coreutils | `cargo +nightly` for `*-unknown-linux-musl`, `-C relocation-model=static`, Rust's or zig's musl ([libc.md](libc.md#static-musl)); each port's `build.sh` owns its toolchain details |
| **Cross-built from source, dynamic musl** | COSMIC, Mesa, libinput/xkbcommon/pixman, pop-launcher, xdg-desktop-portal-cosmic | the shared toolchain in the artifacts tree (`build-rust.sh`, `gen-cargo-config.sh`, merged sysroots); meson cross files for C libraries |
| **Repackaged from Alpine 3.21** | Firefox, PipeWire + WirePlumber, the xdg-desktop-portal frontend, the GPU Mesa stage | `apk add` inside Alpine 3.21 of the target architecture (a podman/docker container on the host, a chroot staged with `apk.static` inside LeandrOS — see [In-guest builds](#in-guest-builds-apk-without-containers)), collect the `DT_NEEDED` closure, rewrite the ELFs for the guest (libc soname, the `__stack_chk_guard` shim), write a rootfs-shaped tree to `ports/<name>/out/<arch>/` that mkfs overlays |

Repackaging was chosen where compiling the upstream C/C++ stack would have been the whole
project (Gecko, GLib): the same binaries run on Linux, and LeandrOS's job is to be Linux enough
for them. Container ports are stamp-skipped while `out/<arch>/.stamp` is newer than the port
sources; without a working container tool, `build-all.sh` reuses a previously packaged tree
and otherwise stops loudly unless `LEANDROS_SKIP_<NAME>=1` — an image silently missing its
audio stack or portal is worse than a clear error. A container tool must *answer* within
`LEANDROS_CT_TIMEOUT` (20 s), so a hung daemon is a fast failure, not a build that waits
forever.

### In-guest builds: apk without containers

The long-term goal is building LeandrOS inside LeandrOS, and there are no containers in the
guest. No port calls podman/docker directly; each runs its Alpine step through
`leandros_alpine_run [--arch A] [--image I] [-e K=V]... [-v HOST:DIR[:ro]]... -- CMD`
in [`scripts/container-lib.sh`](../../scripts/container-lib.sh). That helper has two backends:

- **container**, on the host: the same `run --rm --platform ... alpine:3.21` command line as
  before. Cross-arch runs use binfmt/qemu-user.
- **local**, chosen automatically when `uname -s` is `Leandros` (or `/etc/os-release` has
  `ID=leandros`). `LEANDROS_ALPINE_BACKEND=container|local` overrides the choice. The first
  run stages a base root matching the `alpine:3.21` image under `$LEANDROS_ALPINE_DIR`
  (default `/var/lib/leandros-alpine/<arch>-base`). Each run then copies that root, copies
  the `-v` directories into it, and runs `CMD` under `chroot` with a clean environment.
  Afterwards it copies the writable mounts back. The helper predates bind mounts (the
  kernel now has `mount --bind` and `mount -t proc|devtmpfs|sysfs` into a chroot), so
  copying is still how its `-v` mounts work. Deletions made inside a run are not propagated back. Only the
  native architecture works, because the guest has no qemu-user. A foreign `--arch` fails
  with a clear message. The local backend needs root, because `chroot(2)` requires euid 0.

Inside the chroot, everything is Alpine's own userland, including Alpine's musl as
`/lib/ld-musl-<arch>.so.1`. The scripts written for the container therefore run there
unchanged. They need neither the soname rewrite nor the `__stack_chk_guard` shim.

[`ports/apk`](../../ports/apk/) ships what the local backend needs. `build.sh` fetches the
files from `dl-cdn.alpinelinux.org` on the host and keeps them in `~/.cache/leandros/apk`.
apk-tools-static and alpine-keys are pinned by version and sha256. It runs as a
`build-all.sh` step and needs no container. mkfs overlays the results:

| Image path | What |
|---|---|
| `/usr/sbin/apk.static` | apk-tools 2.14.6, statically linked: it runs on LeandrOS as-is |
| `/usr/sbin/alpine-sysroot` | `alpine-sysroot [--arch A] ROOT PKG...`: seeds `ROOT/etc/apk` with the keys, repositories and `resolv.conf`, then runs `apk.static --root ROOT [--initdb] --arch <native> add PKG...` |
| `/etc/apk/keys`, `/etc/apk/repositories` | the Alpine release keys, and 3.21 main + community over https |
| `/etc/ssl/certs/ca-certificates.crt`, `/etc/ssl/cert.pem` | Alpine's CA bundle. The image had no CA store before; Firefox uses NSS's own |

The image deliberately has no `apk` command. A bare `apk.static add` without `--root` would
install Alpine's musl over the guest's `/lib/ld-musl-<arch>.so.1`, which is the image's own
`libc.so`.

Running Alpine binaries *outside* a chroot, against the guest's libc, is a separate matter:

- `DT_NEEDED libc.musl-<arch>.so.1` needs neither a rewrite nor a symlink for the dynamic
  linker. musl's `load_library` treats any `libc.*` name as itself (the reserved-name check
  in `ldso/dynlink.c`), and the guest's interpreter is that same musl.
- The real obstacle is aarch64's `__stack_chk_guard`, which the guest `libc.so` does not
  export. This is the zig/ld.lld visibility problem recorded in the musl-dynamic notes.
  Until libc exports it, use `LD_PRELOAD=/usr/lib/libleandros_ssp.so.1` for such binaries.
  The repackaged ports keep their `patchelf` rewrite, which is still the right tool for
  shipping into the image.
- This was verified in-guest on 2026-10-12. Alpine's `make` run from an unjailed root works
  on x86_64. On aarch64 it fails with `__stack_chk_guard: symbol not found`, and works with
  the preload.

Verified on 2026-10-12 on both architectures, aarch64 under HVF and x86_64 under TCG on the
Mac:

- `/usr/sbin/alpine-sysroot` staged a base root over https. That covered DNS, TLS through
  the shipped CA bundle, and key verification.
- Package scripts and triggers ran inside the root.
- `leandros_alpine_run` then ran `apk add gcc musl-dev make`, `gcc` and the built binary
  inside the chroot, and copied `/out` back. That took about 40 s on aarch64.
- A foreign `--arch` fails with exit status 3.
- The kernel itself builds in such a chroot with the pinned nightly (2026-10-13). See
  [Self-hosting](../development/selfhost.md).

Making this work needed four kernel fixes in the same lane:

- `PT_INTERP` resolved inside the chroot.
- Shebang script paths made jail-relative.
- `fchdir` kept inside the jail.
- aarch64 `open(2)` flags translated, because `O_LARGEFILE` read as `O_NOFOLLOW`.

It also needed one f2fs fix: `rename` keeps `DT_LNK`.

Known gaps:

- **`/proc` inside a chroot.** It does not appear, and `mount -t proc` onto the root fails.
  Busybox `readlink /proc/self/exe` fails there.
- **Path length.** Paths are limited to 255 bytes, and that limit includes the jail
  prefix, so keep `LEANDROS_ALPINE_DIR` short.
- **f2fs symlinks to tmpfs.** An f2fs symlink whose target is on tmpfs (`/tmp`) does not
  resolve.
- **`/usr/sbin` on `PATH`.** The root shell's `PATH` is only `/bin`, so call
  `/usr/sbin/alpine-sysroot` by full path.
- **Copy cost.** Each run copies the base root (about 7 MiB) and every `-v` directory.

## Port index

| Port | Contents | Notes |
|---|---|---|
| [mesa](../../ports/mesa/) | the GPU Mesa ship-set: Zink, virgl and softpipe gallium drivers (+ v3d for the Raspberry Pi 5), the Venus Vulkan ICD and loader, built in an Alpine container; two patches; `gpuprobe` | [NOTES.md](../../ports/mesa/NOTES.md), [../graphics/mesa.md](../graphics/mesa.md) |
| [gl-stack](../../ports/gl-stack/) | GL support libraries, the shared cross toolchain wrappers and meson cross files, and test clients (`kmscube`); the history of the first dynamic binaries | [NOTES.md](../../ports/gl-stack/NOTES.md) |
| [input-stack](../../ports/input-stack/) | libinput, libevdev, mtdev, libxkbcommon, pixman, libdisplay-info, xkeyboard-config, and the **libseat / libudev shims** | [NOTES.md](../../ports/input-stack/NOTES.md), [../input.md](../input.md) |
| [busd](../../ports/busd/) | the session D-Bus broker actually used, and its four patches | [README.md](../../ports/busd/README.md), [../desktop/session.md](../desktop/session.md#busd) |
| [dbus](../../ports/dbus/) | the reference `dbus-daemon` runtime notes, a zbus test client, and the **session package**: `dbus-run-session`, `session.conf`, the `.service` files | [RUNTIME-NOTES.md](../../ports/dbus/RUNTIME-NOTES.md) |
| [sysbus](../../ports/sysbus/) | `leandros-sysbus`: login1, locale1, UPower, power-profiles, PolicyKit1, RealtimeKit1, and the `probe` client | [README.md](../../ports/sysbus/README.md) |
| [greetd](../../ports/greetd/) | upstream greetd, its config, its launchers, and `pam-leandros` — the libpam application ABI `pam-sys` binds to, backed by `/etc/shadow` | [README.md](../../ports/greetd/README.md), [../desktop/login.md](../desktop/login.md) |
| [cosmic-greeter](../../ports/cosmic-greeter/) | greeter build configuration (`--no-default-features`), and why there is no patch | [README.md](../../ports/cosmic-greeter/README.md) |
| [cosmic-session](../../ports/cosmic-session/) | session build recipe, and the retired `env_rx` patch with its retirement measurement | [README.md](../../ports/cosmic-session/README.md) |
| [cosmic-comp](../../ports/cosmic-comp/) | the one approved COSMIC patch (closed-window texture leak) and its build | [README.md](../../ports/cosmic-comp/README.md) |
| [cosmic-wgpu](../../ports/cosmic-wgpu/) | rebuilds the libcosmic clients with iced's wgpu renderer, by feature flags only | [build-wgpu-clients.sh](../../ports/cosmic-wgpu/build-wgpu-clients.sh) |
| [pop-launcher](../../ports/pop-launcher/) | the launcher's search backend, unmodified | [README.md](../../ports/pop-launcher/README.md) |
| [portal](../../ports/portal/) | xdg-desktop-portal frontend (Alpine) + xdg-desktop-portal-cosmic (cross-built, unmodified) | [build.sh](../../ports/portal/build.sh), [../desktop/session.md](../desktop/session.md#the-desktop-portal) |
| [apk](../../ports/apk/) | `apk.static`, `alpine-sysroot`, Alpine keys/repositories and a CA bundle for in-guest builds; `claude-code-install` and its `claude` launcher | [build.sh](../../ports/apk/build.sh), [In-guest builds](#in-guest-builds-apk-without-containers), [below](#the-claude-code-cli) |
| [pipewire](../../ports/pipewire/) | PipeWire 1.2.7 + WirePlumber from Alpine, the `leandros-snd-sink` virtio-snd sink, `pipewire-pulse`, `cosmic-applet-audio`, a ScreenCast-enabled `cosmic.portal` | [build.sh](../../ports/pipewire/build.sh), [../audio.md](../audio.md) |
| [iso-codes](../../ports/iso-codes/) | Debian iso-codes 4.18.0 JSON tables (sha256-pinned) for Settings' Region page | [build.sh](../../ports/iso-codes/build.sh) |
| [kbd-keymaps](../../ports/kbd-keymaps/) | binary console keymaps for `loadkmap` | [README.md](../../ports/kbd-keymaps/README.md) |
| [firefox](../../ports/firefox/) | Alpine's prebuilt Firefox, its closure, FFmpeg and libpulse, the launcher and prefs | [below](#firefox) |
| [brush](../../ports/brush/) | the pin and patches for the sibling brush checkout, and `sync.sh` | [README.md](../../ports/brush/README.md), [below](#brush) |
| [mkfs-fat](../../ports/mkfs-fat/), [mkfs-xfs](../../ports/mkfs-xfs/) | in-tree Rust formatters for disks-rs | [mkfs-xfs README](../../ports/mkfs-xfs/README.md), [below](#disks-rs-mkfsfat-and-mkfsxfs) |
| [openrm](../../ports/openrm/) | NVIDIA's open GPU resource manager built freestanding for the x86_64 kernel (feature `nvidia`) | [../hardware/pc.md](../hardware/pc.md) |

## Design notes by port

### The graphics and input foundation

- **gl-stack** records how the first dynamic binaries came to exist: a merged sysroot per
  architecture (musl `libc.so` + `ld-musl`, libdrm, Mesa, libwayland, libffi, the input
  libraries), `zig cc` as the C compiler with ld.lld as the linker, and an acceptance check
  that every output is `ET_DYN` with `PT_INTERP=/lib/ld-musl-<arch>.so.1` and zero host
  libraries in its `DT_NEEDED` closure. `kmscube`, smithay's `anvil` and a first cosmic-comp
  link were the rungs. cosmic-comp's smithay feature set is hard-wired in its
  `[dependencies]`, not gated by its own features, so `--no-default-features` drops only
  systemd/logind.
- **input-stack**'s two shims are the reason COSMIC needs no seatd or udev daemon. The
  **libseat shim** implements the `libseat-sys` imports over plain `open`/`close` of device
  nodes for a single `seat0`, puts the seat's VT into `KD_GRAPHICS`, and follows VT switches;
  the **libudev shim** serves a small, data-driven static device model of the QEMU device set
  (`card0`, `renderD128`, the evdev nodes) in place of udevd and netlink uevents. Both are C
  shared objects, because their consumers bind a C ABI, and need only `libc.so`. Their limits — no seatd broker, so the
  greeter's compositor must stay root — are recorded in
  [../desktop/limitations.md](../desktop/limitations.md#login-and-privilege).
- **mesa**: Mesa 25.3.6, no LLVM. The early softpipe-only stage was built on the Mac with
  `zig cc`; the current GPU stage (Zink, virgl, softpipe, the Venus ICD, optionally v3d and
  NVK) is built natively in an Alpine container. Patch `0001` makes EGL report a missing
  robust-access extension as `EGL_BAD_ATTRIBUTE`, so wgpu-hal retries without robustness on
  hosts like ANGLE/Metal; patch `0002` stops virgl emitting triangle fans on GLES hosts,
  because ANGLE-on-MoltenVK draws only the first triangle of a fan (which painted magenta
  over Firefox and settings windows on the Mac).

### The desktop

- **cosmic-greeter / cosmic-session / cosmic-comp** document the zero-patch policy in
  practice: the greeter's patch replaced by a staging decision, the session's patch retired
  after two kernel fixes with a before/after measurement, and the single approved compositor
  patch with its upstream references and a "remove when upstream merges" note
  ([../desktop/README.md](../desktop/README.md#zero-cosmic-source-patches-with-one-approved-exception)).
- **cosmic-wgpu** turns on libcosmic's `wgpu` feature for the greeter, launcher,
  notifications, OSD, workspaces and files applet (Settings, App Library and the terminal
  already had it). wgpu's Vulkan and GLES backends are `dlopen`ed, so no `DT_NEEDED` changes;
  `/bin/gpu-env` decides the backend at run time. Panel applets stay on tiny-skia for the
  reason in [../desktop/session.md](../desktop/session.md#the-panel-and-its-applets).
- **busd** carries the `ServiceUnknown`, activation and unique-activation patches. Its README
  still holds an early investigation ("W1 tokio wedge") whose conclusion was later overturned
  ([../desktop/session.md](../desktop/session.md#the-historical-busd-wedge)); and a changed
  patch needs `rm -rf ports/busd/.work`, because patches are applied only on first extract.
- **dbus**: the reference `dbus-daemon` and a static zbus client were runtime-verified in
  Alpine containers on both architectures before anything was booted, which is how busd's
  routing was known to work on Linux when LeandrOS-side symptoms appeared.
- **greetd**: three patches (`/proc/self/exe` re-exec, fakegreet's `canonicalize`, the
  socket directory), `pam-leandros` as an rlib with two empty archives satisfying `pam-sys`'s
  link lines, and a Landmines section worth reading before touching the login path
  ([../desktop/login.md](../desktop/login.md)).
- **pop-launcher**: built at exactly the revision `cosmic-launcher` pins, because the JSON
  wire protocol has no version field; plugins as hardlinks to the multicall binary.
- **portal** and **pipewire** are both Alpine repackages plus one cross-built or compiled
  LeandrOS piece (the COSMIC backend; the virtio-snd sink). The real `libpipewire-0.3.so.0`
  replaces an inert stub that cosmic-settings-daemon and the portal backend were originally
  linked against (same 1.2.7 ABI, no rebuild). PipeWire's sink is the graph driver, timed like
  spa-alsa's timer-based scheduling against a `GET_DELAY` ioctl, rather than a blocking writer
  that would have kept the kernel spool full and spun a CPU.
- **sysbus** is one binary with one subcommand per service, so busd's `.service` files are
  the whole configuration; mkfs refuses an image whose `.service` `Exec=` is missing or stale.

### Data ports

- **iso-codes**: only the three tables locales-rs reads (~2 MB), from a sha256-pinned Debian
  tarball.
- **kbd-keymaps**: generated with kbd 2.10.0's `loadkeys -b`; `de` is deliberately an alias
  of `de-latin1`, because kbd's own `de` is the 7-bit DIN 66003 layout nobody writing
  `KEYMAP=de` on a UTF-8 system wants. The kernel's built-in map is Linux's
  `defkeymap.map`.

## Firefox

Firefox 136 runs on both architectures as a native Wayland client under COSMIC, with
hardware WebRender on the GPU path, HTTP/2 and HTTP/3 browsing, FFmpeg media and PipeWire
audio.

**Gecko is not compiled.** [ports/firefox/build.sh](../../ports/firefox/build.sh) `apk add`s
Alpine 3.21's prebuilt Firefox (the release the GPU Mesa stage is built on) in a container of
the target architecture, collects its GTK3/NSS closure and fonts, rewrites the ELFs for the
guest, and writes `ports/firefox/out/<arch>/` (~290 MB), which mkfs overlays. Because nothing
compiles, the foreign architecture can run under emulation in a few minutes. Libraries
Firefox `dlopen`s rather than links — system FFmpeg (`libavcodec`/`libavutil`) and `libpulse`
— are invisible to a `DT_NEEDED` walk and are staged separately (`--dlopen-only`). Without
FFmpeg, Firefox fell back to Cisco's OpenH264 plugin, a glibc build whose load failure
crashed the media process; without `libpulse` there was no audio backend.

`/bin/firefox` ([firefox.sh](../../ports/firefox/firefox.sh)) refuses to start without a GPU
renderer rather than let WebRender fall back to software, and
[leandros-prefs.js](../../ports/firefox/leandros-prefs.js) forces WebRender on, disables the
content sandbox level and Fission (one content process), and turns off telemetry, updates and
first-run pages.

**Fewer disk writes (lane quietio, lane ffchurn).** On the PC's USB 2.0 stick a write command costs up to 1 s and stalls the whole system, so the profile writes as little as it can: the disk cache is off (`browser.cache.disk.enable` false, memory cache 256 MiB), session store snapshots every 5 minutes instead of 15 s, Safe Browsing list downloads, telemetry, Normandy/studies, search and add-on update checks, speculative prefetch and Contile are off. `security.remote_settings.intermediates.enabled=false` stops the intermediate-CA preloading churn: on a new profile Remote Settings fetches the 1771 certificate attachments of `security-state/intermediates` and caches each as an IndexedDB Blob, i.e. one file and one `fsync` per certificate (PC boot 17: files 1..380, one about every 5 s, each fsync a multi-second F2FS checkpoint). Measured on a new profile in the first 6 minutes in QEMU on example.com: aarch64 4568 to 623 fsyncs and 1993 to 49 blob files, x86_64 4533 to 671. The certificates only help TLS servers that send an incomplete chain. Two prefs that did nothing were dropped: `services.settings.server=""` (release builds ignore any override of the server URL, so it never turned Remote Settings off) and `toolkit.storage.synchronous` (the pref does not exist in Firefox 136; IndexedDB runs WAL with `synchronous=NORMAL`, so a commit does not fsync, only WAL checkpoints and Blob files do). With `f2fs.fsync=lazy` ([filesystems.md](../servers/filesystems.md#performance-on-slow-media)) the remaining fsyncs return at once.

Bringing it up was a sustained kernel-compatibility exercise; the instructive defects were:
the content process `_exit(127)`ing because `dup3` onto fds 5/6 of a socket fd was routed to
the VFS; SCM_RIGHTS of connected unix ends; `mmap` hints above 128 TiB; `ARCH_SET_GS`; a
`sendmsg` short write skipping iovecs and corrupting the stream; `RFLAGS.DF` not cleared on
x86_64 kernel entry (kernel copies ran backwards after a musl `memmove`); `MADV_DONTNEED` being
a no-op (Scudo's `calloc` returned stale memory); `CNTKCTL_EL1.EL0VCTEN` never set on aarch64
(`SIGILL` on `mrs cntvct_el0` in libjxl); a per-QEMU NIC MAC (two guests on one host shared a
MAC and reset each other's TCP); and a shared page cache for clean file pages, which brought
Firefox's working set on virgl from 4 GiB down to 2 GiB
([lane-firefox](../../artifacts/notes/lane-firefox-2026-09-27.md)).
Firefox's JIT keeps W^X: the kernel policy was kept and the content process uses Firefox's own
write-protect-code preference.

## brush

[brush](https://github.com/reubeno/brush) is built from the sibling checkout `../brush`
(override `LEANDROS_BRUSH_DIR`), not fetched into a work tree. That checkout must be upstream
`e46b4ae` plus exactly the patches in [ports/brush/patches](../../ports/brush/patches/):
`sync.sh check` verifies it (clean tree, diff against the pin equals the patches), `sync.sh
apply` resets a checkout to that state, and `build_brush` refuses an off-pin tree unless
`LEANDROS_BRUSH_UNCHECKED=1`. The check exists because one machine's checkout once lagged a
patch, so only one architecture's images had a `/bin/sh` whose `cmd & exec other` could drop
`cmd` — PipeWire went missing from aarch64 sessions only.

| Patch | Fixes |
|---|---|
| `0001` | wires the sibling `../crossterm` fork (0.29.0 plus a cursor-position-report desync fix) through `[patch.crates-io]`; the `kill` builtin (default SIGTERM, numeric `-s N`, `kill -0` as a probe — `dbus-run-session` relies on it); a background-job pid slot |
| `0002` | `consume_budget()` at the top of every loop iteration, so N builtin-only background loops cannot occupy all N tokio workers and freeze the shell |
| `0003` | job control: `fg`/`bg`/`kill %n` signal the whole process group; the terminal is given back on every pipeline failure path; `cmd &` announces a real `[n] pid` and `$!` |
| `0004` | `TryFrom<OpenFile> for Stdio` duplicates the shell's stdio instead of inheriting, so `cmd >&2` and `2>&1` go where they should when the shell's own stdio is redirected |

All of these reproduce on Linux; each patch adds upstream tests (`brush-interactive-tests`,
compat cases compared against bash), and the guest-side check is `scripts/shjobs.py`. Known
upstream limits remain: a background job whose first command is a builtin or a loop has no pid
(`$!` empty, `kill %n` fails); `jobs` shows a job killed while stopped as `Stopped` until the
next prompt; `wait <pid>` is not implemented; trapped signals (`trap … TERM`) are stored but
never delivered.

## DOOM

`doom` is [doomgeneric](https://github.com/ozkl/doomgeneric) built from the sibling
`../doomgeneric` against leandros-libc (static, `*-unknown-none`), with the shareware WAD.
Because that sibling is a shared, non-git checkout that every worktree on a machine resolves to
the same directory, the LeandrOS platform sources and Makefile live in
[scripts/vendor/doomgeneric](../../scripts/vendor/doomgeneric/) and are copied over the
sibling's whenever they differ; objects go to a per-worktree, per-arch directory and the binary
is linked atomically, and the image takes DOOM from the worktree's own `target/doom/`, never
from the shared path (which belongs to whichever tree linked last — a stale relink there once
resolved `environ` to NULL).

Under COSMIC, DOOM draws as a Wayland client (`leandros_wayland.h`) and plays sound by piping
to `pw-cat`, because PipeWire holds the single-writer audio device. On a text VT it presents
through KMS, or `/dev/fb0` as a fallback; DRM master is per VT, so both can run at once. A raw
KMS program started from a terminal inside the desktop is invisible on Linux too, which is why
the Wayland path exists rather than a kernel exception
([lane-doomvis](../../artifacts/notes/lane-doomvis-2026-10-06.md)). DOOM's General MIDI music
uses a pinned SoundFont. x86_64 audio clipping, from DOOM's own mixing, is open.

## MAME

MAME is built from the sibling `../mame` with a LeandrOS OSD (`src/osd/leandros/`) and
`Makefile.leandros`: GENie project generation, `zig` as the C/C++ toolchain targeting
`<arch>-linux-musl` for the C++ runtime, compiled against relibc's sysroot. Points worth
knowing before touching it: GENie needs `--PLATFORM` even for non-Android builds and an empty
`ARCH=` in the sub-make; `USE_QTDEBUG` must be forced to 0 before `modules.lua` runs, with a
stub `qtdbg_leandros` project satisfying the unconditional link; musl's `PAGE_SIZE` macro
collides with MAME member names; version symbols need `extern const char[]`. The unstripped
binary is ~2.8 GB. ROM sets are zips in `/` (`mame -rompath / <game>`).

MAME's sound driver writes PCM to the kernel audio server over IPC (port discovered from the
auxv). Getting clean audio found two real bugs, neither the predicted throughput problem: no
backpressure anywhere (everything beyond the ring and spool was silently dropped — fixed with a
blocking push, which also paces MAME to exactly 100 % speed), and QEMU permanently disabling a
virtio-snd stream that underruns, recovered by a stall detector that restarts the stream
([../audio.md](../audio.md)).

## disks-rs, mkfs.fat and mkfs.xfs

AerynOS's [disks-rs](https://github.com/AerynOS/disks-rs) runs **unmodified**: its `disktester`
creates a 32 GiB sparse image, attaches a loop device, writes GPT, syncs partitions with
`BLKPG`, and formats them with `mkfs.fat` (twice) and `mkfs.xfs`. Its OS contract is small —
`/sys/class/block/<name>/…` read with `read_dir`/`read_to_string`, `/dev/loop-control` and the
loop ioctls, `BLKPG` add/delete, `seek(End(0))` for device size, `fsync` — and the kernel side
of it is the VFS block layer ([../servers/filesystems.md](../servers/filesystems.md)).

The formatters are in-tree Rust because the repository is Rust and the originals are C:

- **mkfs.fat** ([ports/mkfs-fat](../../ports/mkfs-fat/)) wraps the `fatfs` crate behind a
  dosfstools-compatible CLI, FAT32 forced.
- **mkfs.xfs** ([ports/mkfs-xfs](../../ports/mkfs-xfs/README.md)) is an XFS v5 writer from
  scratch: the feature set `mkfs.xfs` 7.x enables by default plus exchange-range and parent
  pointers, geometry computed exactly as xfsprogs does so the superblock is byte-identical to
  the real tool's for a given size, every field confirmed against images written by xfsprogs
  7.1.1 and checked clean by `xfs_repair -n`. It follows libxfs's growfs path
  (`xfs_ag_init_headers`) rather than mkfs's transactional one. `XFS_SB_VERSION2_FTYPE` must not
  be set on v5; it refuses devices under 300 MiB or fewer than two AGs.

`disktester` loads its KDL scenario relative to the working directory, so it is run as
`cd /root/disks-rs && disktester`; "failed to parse KDL" means the file was not found.

## The Claude Code CLI

The Claude Code CLI runs unmodified on both architectures. What works: `claude --version`,
the interactive onboarding and login screens with working keys, and HTTPS to
`api.anthropic.com`. The interactive screens were verified on the text VT and in a tmux pty;
cosmic-term has not been tried yet.

**There is nothing to build.** The npm package `@anthropic-ai/claude-code` carries no
JavaScript any more: its `bin/claude.exe` is a stub, and its postinstall copies in the native
executable from a platform package. `linux-arm64-musl` and `linux-x64-musl` hold one Bun
(JavaScriptCore) executable of about 250 MB. It is dynamically linked against musl and
needs only `libc.musl-<arch>.so.1`, so it runs in an Alpine root as it is. Neither node
nor npm is involved.

```sh
/usr/sbin/claude-code-install   # as root: Alpine root /opt/alpine + the pinned release
/usr/bin/claude --version       # 2.1.296 (Claude Code)
/usr/bin/claude                 # the TUI
```

- **[claude-code-install](../../ports/apk/claude-code-install.sh).**
  - It stages `/opt/alpine` with `alpine-sysroot`. The packages are bash (the CLI's shell),
    curl, ca-certificates, ripgrep (its search tool) and git.
  - It downloads the musl package from `registry.npmjs.org` and checks it against the
    sha512 that npm publishes for the pinned version.
  - It unpacks the executable to `/opt/alpine/opt/claude-code/<version>/claude`. A
    *relative* link `usr/local/bin/claude` points at it, so the link resolves the same way
    inside and outside the root.
  - It installs the launcher as `/usr/bin/claude`.
  - Set `CLAUDE_CODE_VERSION` and `CLAUDE_CODE_SHA512` to pin another release.
  - The install takes about 20 s on aarch64 (HVF) and about 50 s on x86_64 (TCG). The root
    and the executable live on the root disk, so they persist across reboots.
- **[The launcher](../../ports/apk/claude-code-run.sh).**
  - It mounts `/proc`, `/dev` and `/tmp` into the root. Bun reads `/proc/self/exe` and
    aborts without it. The terminal is a `/dev/pts` node. AF_UNIX sockets bind only on
    tmpfs.
  - It then enters the root with `chroot` and a clean environment (`SHELL=/bin/bash`,
    `TERM` and `COLORTERM` passed through) and runs the CLI.
  - `chroot` and `mount` need root, and the image has no `su`, so the CLI runs as root.
- **No credential is ever placed in the image.** The network check
  ([scripts/claudecode-smoke.sh](../../scripts/claudecode-smoke.sh), step 5) uses a made-up
  API key and expects the server's `401 authentication_error`.

`scripts/claudecode-smoke.sh <arch>` is the headless end-to-end check. It boots, installs,
runs `claude --version`, drives the TUI in a pty through tmux, and makes the network check.
For the TUI it captures the screen with `capture-pane` and sends keys with `send-keys`:
Down moves the theme selection, and Enter reaches the login-method screen.

Kernel defects the bring-up found:

| Defect | Effect | Fix |
|---|---|---|
| preadv, pwritev, preadv2 and pwritev2 were ENOSYS. | Bun reads a non-blocking stdin with `preadv2(0, .., -1, RWF_NOWAIT)` and falls back to poll + read only on EOPNOTSUPP. The TUI drew its first screen and then ignored every key. | Linux semantics. RWF_NOWAIT is EAGAIN on an empty pipe or socket, and EOPNOTSUPP on terminals and devices. |
| AF_UNIX `bind` and `connect` used `sun_path` verbatim, as a host path. | A chrooted tmux could not create `/tmp/tmux-0/default`. A relative name never resolved against the cwd. | The kernel resolves the name the way `open` does: cwd, chroot and alias mounts. getsockname now returns the bound name instead of an unnamed address. |
| setfsuid and setfsgid were ENOSYS. | tmux calls them. | Both answer the effective ids. |
| After a pty hangup, `/dev/tty` opened the serial console. | A CLI left running by a killed tmux read the console and took every other keystroke from the login shell. | ENXIO, as on Linux. |
| The console font drew `?` for everything outside ASCII. | The onboarding art and the selection marks were unreadable. | Generated box-drawing, block and shade glyphs, plus bitmaps for the common UI symbols ([drivers/src/glyphs.rs](../../drivers/src/glyphs.rs)). |

## See also

- [README.md](README.md), [libc.md](libc.md), [programs.md](programs.md)
- [../desktop/README.md](../desktop/README.md) — how the COSMIC ports fit together
- [../graphics/mesa.md](../graphics/mesa.md) — Mesa drivers and patches in depth
- [../getting-started/building.md](../getting-started/building.md) — sibling repositories and build flags
