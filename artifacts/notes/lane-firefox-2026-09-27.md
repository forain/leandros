# Lane firefox — 2026-09-27

Branch `lane/firefox` (local, not pushed), base `7b28aa05`. One commit: "ports: stage Alpine's prebuilt Firefox as an optional image component".

## Verdict
- **Build and staging: DONE on both arches.** `build-all.sh` stages Firefox the way it stages doom and MAME. It is skipped with a `⚠️` warning when docker/podman is missing, the daemon is down or the build fails. If the output is newer than the port scripts it takes under a second.
- **Boot: no regression.** Both arches boot to the COSMIC desktop with Firefox in the image. The greeter login, panel, dock and cosmic-term all work. The desktop keeps running after Firefox dies. aarch64 was tested on HVF, x86_64 on TCG, both with virgl on the Mac GPU QEMU (gles31) and 4 GiB of guest memory.
- **Runtime: Firefox does not open a window.** It loads, links, connects to cosmic-comp and receives the dmabuf feedback. It then dies inside SpiderMonkey init with exit status 139 (SIGSEGV) and prints nothing. The first failure differs by arch (see below). Both are kernel issues, which are out of scope for this lane.

## What ships
- `ports/firefox/build.sh <arch|all>` runs on the host. It picks podman, else docker, and runs `alpine:3.21` with `--platform linux/arm64|amd64`. It follows the conventions of `ports/mesa/build-gpu-stack.sh`: a snapshot of the scripts, a per-arch log, and `=== rc=N arch=A ===` as the last line. On success it touches `out/<arch>/.stamp`.
- `ports/firefox/build-in-alpine.sh <arch>` runs in the container:
  - `apk add firefox` plus fonts, fontconfig, gdk-pixbuf, gtk3, nss and pciutils-libs.
  - Walks the DT_NEEDED closure of every ELF under `/usr/lib/firefox` and the pixbuf loaders. It also adds the libraries Firefox loads with dlopen: the NSS PKCS#11 modules, libpci, libgcc_s, libepoxy, libxkbcommon and libwayland-cursor.
  - Stages everything under the soname, with symlinks dereferenced.
- **Not shipped:** Alpine's Mesa (EGL, GLES, GL, gbm, glapi, gallium), libdrm, libvulkan, libwayland-{client,server,egl} and libudev. These always come from the image.
- **ELF fixes:**
  - `libc.musl-<arch>.so.1` is rewritten to `libc.so`.
  - PT_INTERP is left as `/lib/ld-musl-<arch>.so.1`, which is where the image already packs libc.so.
  - `libleandros_ssp.so.1` is added to DT_NEEDED of every ELF that imports `__stack_chk_guard`. The check reads each ELF's symbols: 98 ELFs on aarch64, 0 on x86_64.
- **libscudo dropped.** Alpine links the firefox launcher against scudo. scudo has a 512 MiB `.bss`, so musl's first mmap over the whole library is about 512 MiB. The kernel caps file mmaps at 256 MiB, so that mmap fails and loading stops with `Error loading shared library libscudo.so: Invalid argument (needed by /usr/lib/firefox/firefox)`. `patchelf --remove-needed libscudo.so` makes malloc come from the guest libc.
- **Symbol audit (`out/<arch>/SYMCHECK.txt`).** Every strong undefined symbol of every shipped ELF is checked against the guest's `libc.so`, the staged tree and the image's own copy of each clashing soname. Result: **0 unresolved on both arches.** A negative control shows libxul alone has 2364 symbols that libc does not define, so the audit is not empty by construction.
- **Launcher `/bin/firefox`** (`ports/firefox/firefox.sh`, `#!/bin/sh` = brush):
  - Sources `/bin/gpu-env` and refuses (exit 78) without zink or virgl. GPU rendering only.
  - Sets `MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland MOZ_DISABLE_WAYLAND_PROXY=1 MOZ_ACCELERATED=1`.
  - Disables every `MOZ_DISABLE_*_SANDBOX` (the kernel has no seccomp and no namespaces), the crash reporter, the a11y bridge, dconf and portals.
- **Prefs** (`/usr/lib/firefox/defaults/pref/leandros-prefs.js`): force hardware WebRender, disable the software-WebRender fallback, sandbox level 0, a single content process with no fission, and no network on startup.
- The launcher and prefs are staged by mkfs directly from `ports/firefox`, so editing them needs no container run.
- **mkfs** overlays `ports/firefox/out/<arch>/` only if `usr/lib/firefox/libxul.so` exists:
  - Any `usr/lib` soname the image already packs keeps the image's copy: libexpat, libffi, libz, libzstd, libxkbcommon, libpixman and libleandros_ssp.
  - Host-executable files are packed 0755.

## Versions and sizes
| | aarch64 | x86_64 |
|---|---|---|
| Firefox | 136.0.4-r0 (Alpine 3.21.7 community) | 136.0.4-r0 |
| staged tree `out/<arch>` | 287 MB | 291 MB |
| packed into the image (after the image-copy dedupe) | 187 files, 283 MiB | 186 files, 288 MiB |
| F2FS image without Firefox → with Firefox | 2580 → 3152 MB (**+572 MB**) | 2588 → 3168 MB (**+580 MB**) |

- Alpine 3.21 matches the Mesa stack: musl 1.2.5, GCC 14.2 libstdc++, wayland 1.23.1.
- Image sizing is automatic: segments = 2 × content, to keep the 50% free-space policy. So about 285 MiB of content grows the image by about 575 MB. The root filesystem is at 51% use after boot. No code change was needed.
- libxul (143 MB, about 35k blocks) goes through the image writer's indirect-node path, which already existed.
- These baselines come from this worktree, which has no `../disks-rs` symlink, so both baselines lack disktester.

## Runtime, exactly (committed launcher, from cosmic-term in a greeter-started `leandro` session)
The kernel does not log a fault that a user signal handler takes, and Firefox installs one. So the attribution below comes from temporary `[FFDBG]` instrumentation on handled faults, signal delivery, exit and mmap rejections, with serial held open for the whole run. **The instrumentation was reverted and is not in the commit.**

### aarch64 (HVF, virgl)
1. SIGILL in libxul at file offset `0x28A0B80`. ESR `0x6232C101`: EC 0x18, a trapped MRS reading `S3_3_C0_C0_1` = **CTR_EL0** into x8. The kernel runs EL0 with `SCTLR_EL1.UCT=0`, so userspace cannot read the cache-type register. Linux sets UCT, and JITs (cache flushing) rely on it.
2. Firefox's SIGILL handler (`0xE3056950`) cannot be entered: `arch_prepare_signal_frame` fails, and the kernel turns that into SIGSEGV → exit 139.

### x86_64 (TCG, virgl)
1. `mmap(NULL, 0x7FC00000, …, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)` → EINVAL. This is SpiderMonkey reserving its roughly 2 GiB of JIT code space (ProcessExecutableMemory). `ANON_MAP_MAX_BYTES` is 512 MiB.
2. As a result `JS_InitWithFailureDiagnostic()` returns a failure string, and libxul does `MOZ_CRASH` (a `movl $0xfa, 0x0` at file offset `0x2BB4FB5`).
3. The SIGSEGV handler (`0xE2939FF0`) cannot be entered for the same reason as on aarch64: `sigframe-write-failed` → exit 139.

### Both arches, earlier failure (now worked around in the launcher)
- Firefox's Wayland proxy fails before any window:
  - `ProxiedConnection::TransferOrQueue() broken source socket: Bad file descriptor`
  - `ProxiedConnection::Process(): Failed to read data from client!: Bad file descriptor`
  - `Error: we don't have any display, WAYLAND_DISPLAY='wayland-1'`, exit 1
- `MOZ_DISABLE_WAYLAND_PROXY=1` goes around it. GTK then connects directly and receives the full dmabuf format table from cosmic-comp.
- No `[SYSCALL] ENOSYS` line was printed in any run.

## Open (kernel, for a follow-up lane, in the order Firefox meets them)
1. **Signal frames cannot be written** for Firefox's fault handlers on either arch (`check_and_deliver_signals` → `arch_prepare_signal_frame` false). This is probably `SA_ONSTACK` with a lazily-backed sigaltstack. Until it is fixed, every Firefox crash reads as a bare 139.
2. **aarch64: allow EL0 cache-type reads.** Set `SCTLR_EL1.UCT`. Probably also DZE (`DC ZVA`) and UCI (`DC CVAU`/`IC IVAU`, which a JIT needs), or emulate the trap.
3. **mmap size caps:** `ANON_MAP_MAX_BYTES` 512 MiB (SpiderMonkey reserves about 2 GiB `MAP_NORESERVE`) and `MAP_MAX_BYTES` 256 MiB for file mappings (musl maps a library's whole span first). Only libscudo hit the file cap and it was dropped. The largest mapping left is libxul at about 141 MiB (aarch64) / 152 MiB (x86_64).
4. **Firefox's Wayland proxy gets EBADF** relaying between its accepted client socket and the compositor socket on a separate thread. Worked around with `MOZ_DISABLE_WAYLAND_PROXY=1`; the root cause is not investigated.
5. After those: glxtest (a forked child that probes EGL, seen mapping libgallium), content-process launch, and whether WebRender takes our zink/virgl EGL. None of these has been reached yet.

## Notes
- `ports/firefox/build.sh` also works under emulation for the foreign arch. It only runs apk and patchelf, about 3–5 min per arch on the Mac.
- The list of image sonames used by the audit (`image-sonames.txt` in `build.sh`) mirrors mkfs's `usr_lib_files`. Keep them in sync.
