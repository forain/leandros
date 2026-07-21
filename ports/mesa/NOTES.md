# S3 Mesa Cross-Compile Probe — progress log
Workdir: /Users/forain/.claude-forain/jobs/afde2e74/tmp/s3-mesa-probe
Target: x86_64-unknown-linux-musl. Goal: probe whether Mesa (EGL+GLESv2+GBM+softpipe/kms_swrast, no LLVM, no glvnd) cross-compiles on this macOS host. Find blockers cheaply.

## Host tooling (verified)
- zig 0.16.0  -> used as CC via wrapper `zig cc -target x86_64-linux-musl`
- meson 1.11.2, ninja 1.13.2, pkgconf+pkg-config, flex, bison, python 3.14.6
- NO musl-cross, NO cmake, docker NOT running
- toolchain wrappers in ./toolchain/ (cc,c++,ar,ranlib). hello.c -> static x86-64 musl ELF OK. shared .so OK (6.7s first run building musl runtime).

## Sources
- libdrm 2.4.134 (git f198c21) in ./src/libdrm
- mesa: cloning to ./src/mesa (git main/depth1)

## Progress
- [done] toolchain chosen: zig cc
- [next] meson cross file + target pkgconfig sysroot, then build libdrm

## UPDATE 2
- cross file written: ./cross-musl-x86_64.ini (uses zig cc/c++/ar/ranlib; c_ld=lld; pkg_config_libdir -> sysroot/usr/lib/pkgconfig; sys_root set)
- libdrm CONFIGURE OK, BUILD OK -> libdrm.so.2.134.0 (proper linux x86-64 ELF). Cross setup FULLY VALIDATED.
- libdrm installed to ./sysroot (DESTDIR), libdrm.pc present at sysroot/usr/lib/pkgconfig/
- NOTE: meson put pkgconfig at sysroot/usr/lib/pkgconfig (prefix=/usr), cross file pkg_config_libdir must match -> FIXED below
- Mesa: git clone failed (early EOF, heavy repo). Using RELEASE TARBALL mesa-25.3.6 from archive.mesa3d.org instead. Extracting to ./src/mesa
- [next] fix pkg_config_libdir path, configure Mesa minimal

## UPDATE 3 (Mesa configure)
- Mesa 25.3.6, options file is meson.options (not meson_options.txt)
- gallium-drivers choice for SW = 'softpipe' (llvmpipe needs LLVM). NO 'swrast'/'kms_swrast' choice — kms_swrast_dri is produced automatically from a SW gallium driver + gbm/dri.
- REMOVED options that no longer exist in 25.3.6: gallium-va, gallium-vdpau, video-codecs, zstd, libunwind. shared-glapi is DEPRECATED (still accepted).
- zsh eats '-Dplatforms=[]' unless quoted.
- Cross compiler recognized: zig cc = clang 21.1.8, linker ld.zigcc. Build machine = apple clang, aarch64.
- Python: meson uses homebrew python3 3.14.6; needs modules mako + packaging + yaml. Provided via venv site-packages on PYTHONPATH.
- [next] re-run configure with PYTHONPATH exported

## UPDATE 4 — MESA SURFACELESS CONFIGURE: SUCCESS (rc=0)
- Config summary: Gallium softpipe; Platforms surfaceless+drm; GBM enabled (internal); EGL enabled; GLESv2 enabled; Vulkan none; LLVM disabled; glvnd disabled.
- expat + zlib built as meson subprojects (wrapdb) — cross-built fine implicitly during configure checks.
- FULL working configure invocation saved to ./configure-mesa-surfaceless.sh
- [next] ninja build (background), drive as far as it goes.

## UPDATE 5 — wayland delta + build plumbing
- WAYLAND platform needs (mesa/meson.build:2012-2016): wayland-protocols>=1.41, wayland-client>=1.18, wayland-server>=1.18, wayland-egl-backend>=3. Only wayland-protocols has a mesa wrap; libwayland (client/server/egl-backend) has NO wrap -> must cross-build libwayland separately for musl, AND build host wayland-scanner. This is the surfaceless-vs-wayland delta. Deferred, characterized (see blocker list).
- BUILD PLUMBING BUG (not a real blocker): ninja custom codegen commands invoke /opt/homebrew/bin/python3 and need mako importable. `meson setup` PYTHONPATH does NOT propagate to ninja; must export PYTHONPATH before `ninja`. Fixed by exporting venv site-packages. Real fix in prod: pip-install mako/packaging/pyyaml into the python meson uses.
- Rebuild launched with PYTHONPATH exported.

## UPDATE 6 — host bison too old (REAL host blocker, trivial fix)
- macOS /usr/bin/bison = Apple GNU Bison 2.3; Mesa glcpp-parse.y:179 uses `%define api.pure` + `%define parse.error verbose` (needs bison >= 3.0). => `brew install bison` (3.8.2 at /opt/homebrew/opt/bison/bin) and put ahead of /usr/bin on PATH.
- flex 2.6.4 (Apple) is fine.
- Env needed for BOTH `meson setup` and `ninja`: PATH=/opt/homebrew/opt/bison/bin:$PATH and PYTHONPATH=venv-site-packages.

## UPDATE 7 — REACHED 954/959; blocker at megadriver link (zig-cc-specific, moderate, host-side)
- Compilation of the ENTIRE tree succeeded (954/959). Only the final link of libgallium-25.3.6.so (the megadriver) failed.
- Root cause: link line has `-Wl,--version-script <space> /abs/dri.sym`. The flag goes to ld via -Wl, but the .sym PATH is a separate positional token. gcc/clang forward unknown-extension positional files to the linker; ZIG CC rejects them: "unrecognized file extension". zig-cc-specific incompatibility. Recurs for every versioned .so (libEGL/libgbm/libglapi/libGLESv2...).
- FIX applied in wrapper: when a token is exactly `-Wl,--version-script` or `-Wl,--dynamic-list`, consume the next token and merge to `<flag>=<path>` (the =form goes straight to lld as one -Wl arg, no positional classification). Verified with a standalone version-script link test.
- Caveat: wrapper uses unquoted $args reassembly (fine here; workdir path is space-free).
- Resuming incremental ninja.

## UPDATE 8 — SURFACELESS BUILD 100% COMPLETE (ninja rc=0, FAILED=0)
Produced (all verified x86-64 musl ELF), installed to ./stage:
  /usr/lib/libEGL.so.1.0.0, libGLESv2.so.2.0.0, libGLESv1_CM.so.1.1.0, libgbm.so.1.0.0
  /usr/lib/libgallium-25.3.6.so  (MEGADRIVER: softpipe + swrast + kms_swrast winsys, ~single big .so)
  /usr/lib/gbm/dri_gbm.so        (GBM backend)
  deps built as subprojects: /usr/lib/libexpat.so.1, /usr/lib/libz.so.1 ; + sysroot libdrm.so.2
### dlopen / .so coupling (from pyelftools NEEDED + strings)
  - libEGL   NEEDED: libgallium, libexpat, libgbm, libdrm, libc.so(musl)
  - libGLESv2 NEEDED: libgallium, libc.so
  - libgbm   NEEDED: libdrm, libc.so ; DLOPENs dri_gbm.so via GBM_BACKENDS_PATH (default /usr/lib/gbm)
  - dri_gbm  NEEDED: libgallium, libexpat, libdrm, libc.so
  - libgallium NEEDED: libz, libexpat, libdrm, libc.so
  => Modern Mesa (>=24.1): NO per-driver /usr/lib/dri/*_dri.so; the softpipe/kms_swrast driver IS the megadriver libgallium-*.so, linked as hard NEEDED (not dlopen). /usr/lib/dri is EMPTY. The ONLY runtime dlopen is libgbm->dri_gbm.so. We built WITHOUT glvnd, so libEGL/libGLESv2 are the real Mesa libs (no glvnd dispatch/vendor .json/DT_NEEDED libGLdispatch).
  => Target-side note: every lib NEEDs musl `libc.so`. Matches the plan's "dynamic musl userland". LeandrOS must supply musl libc.so + ld-musl (NOT relibc) as the loader for these binaries.

## UPDATE 9 — WAYLAND platform blocker CONFIRMED (concrete)
- `-Dplatforms=wayland` configure FAILS: wayland-protocols subproject (auto-dl 1.41) needs build-time `wayland-scanner`; not found; falls back to a `wayland` subproject; Mesa has NO wayland.wrap -> "Neither a subproject directory nor a wayland.wrap file was found." Hard stop at configure.
- To close: (1) build HOST wayland-scanner (mac) — needs host libexpat+libffi; (2) cross-build libwayland (wayland-client/server/egl-backend) for musl — needs libffi cross-built (no mesa wrap) + expat (have). Both are small/meson. Classification: MODERATE, mostly host-side tooling + 1 extra target dep (libffi).

## FINAL ARTIFACTS in workdir
- cross-musl-x86_64.ini            (meson cross file)
- toolchain/x86_64-linux-musl-{cc,c++,ar,ranlib}  (zig wrappers; cc/c++ normalize --version-script)
- configure-mesa-surfaceless.sh    (configure only)
- build-mesa-surfaceless.sh        (env + configure + ninja, reproducible)
- build/mesa-surfaceless/          (complete build tree, rc=0)
- stage/                           (DESTDIR install; runtime .so layout)
- logs/                            (all setup/build logs)

## VERDICT: surfaceless/GBM Mesa CROSS-COMPILES CLEANLY on macOS via zig cc. 3 host fixes needed (mako/packaging pip, brew bison, wrapper --version-script normalization). Wayland platform = 1 moderate follow-up (libwayland+scanner). No musl-ism blockers hit in Mesa C at all.

================================================================================
# WAVE 2 — Wayland platform + aarch64 (both arches complete)
Workdir: /Users/forain/.claude-forain/jobs/afde2e74/tmp/mesa-wave2 (copy of S3 + extensions)
Versions: wayland 1.23.1, libffi 3.4.6, wayland-protocols 1.41 (mesa wrap), mesa 25.3.6, libdrm 2.4.134.

## RESULT: BOTH GOALS DONE
- x86_64: libEGL/GLESv2/gbm/gallium megadriver + dri_gbm built with -Dplatforms=wayland (EGL_EXT/KHR_platform_wayland). rc=0.
- aarch64: full toolchain extended + libdrm+libffi+libwayland+Mesa all cross-built. rc=0. NO aarch64-specific zig/lld issue.

## ports/ recipe (this dir). One-time: run ./instantiate.sh to expand @DIRNAME@ in the *.ini.
Layout the scripts expect under this dir: src/{mesa,libdrm,wayland,libffi-x86_64,libffi-aarch64},
  .venv (mako/packaging/pyyaml/pyelftools), host/ (native scanner), sysroot-<arch>/, build/, stage-<arch>/.
Order:  instantiate.sh -> build-host-scanner.sh -> per arch: build-libdrm.sh / build-libffi.sh /
  build-libwayland.sh -> build-mesa-wayland.sh.
NOTE: cross-musl-x86_64.ini sys_root was renamed sysroot -> sysroot-x86_64 (per-arch, to coexist with
  aarch64). Put libdrm etc. under sysroot-x86_64 for the surfaceless recipe too.

## NEW host fixes (beyond S3's 3)
4. Wrapper arg-splitting bug (latent in S3): the old `args="$args $1"; exec zig cc $args` rebuild
   word-splits any arg with a space. expat compiles with -DXMLIMPORT=__attribute__ ((visibility("default")))
   -> zig sees `((visibility("default")))` as a positional file: "unrecognized file extension". Fixed:
   rewrite argv in place with `set -- "$@" ...` (toolchain/*-cc,*-c++). Keeps the --version-script=<path>
   normalization. Backward compatible.
5. libtool on a darwin BUILD host cannot emit a Linux ELF .so (libffi): it builds objects + static lib,
   then leaves dangling libffi.so.8 symlinks with no real object. Fix in build-libffi.sh: link the .so
   from the PIC objects with zig cc after `make`.
6. Native wayland-scanner for a cross build needs a NATIVE FILE, not env PKG_CONFIG_PATH: meson clears
   PKG_CONFIG_PATH for native:true deps in a cross build (verified in meson-log). native-host.ini sets
   [built-in options] pkg_config_path -> host/lib/pkgconfig. Same file satisfies Mesa's wayland-protocols
   wrap + wayland-scanner.
7. (not a blocker) wayland-scanner builds NATIVELY on macOS unpatched with -Dlibraries=false -Dscanner=true
   -Ddtd_validation=false (brew expat only). Gives host/bin/wayland-scanner + wayland-scanner.pc.

## EGL_WL_bind_wayland_display on swrast — ANSWER: NOT exposed (double-gated). Evidence (mesa 25.3.6):
- COMPILE gate: meson option `legacy-wayland` (array, DEFAULT []). `-DHAVE_BIND_WL_DISPLAY` is added only if
  it contains 'bind-wayland-display' (src/egl/meson.build:127-132; meson.build:2010 with_wayland_bind_display).
  Without it, dri2_set_WL_bind_wayland_display() (egl_dri2.h:592) is an #ifdef'd no-op -> extension never
  advertised, and wayland-drm protocol + libwayland_drm are not built. => plain -Dplatforms=wayland does NOT
  expose it.
- RUNTIME gate (even with the option, which our build sets): egl_dri2.h:596 sets
  WL_bind_wayland_display = dri2_dpy->has_dmabuf_import && has_dmabuf_export. softpipe/kms_swrast has no
  dma-buf import/export -> FALSE. platform_wayland.c:2721 also needs dri2_dpy->wl_drm + WL_DRM_CAPABILITY_PRIME;
  swrast compositors expose wl_shm, not wl_drm.
Our x86_64/aarch64 libEGL were built WITH -Dlegacy-wayland=bind-wayland-display, so the eglBind/Unbind/
  CreateWaylandBuffer/QueryWaylandBuffer WL entrypoints are PRESENT in .dynsym (compile path proven), but
  they will return unsupported / the extension string will not be in eglQueryString(EXTENSIONS) on a
  software display. => cosmic-panel's bind_wl_display on a nested swrast server: expect UNSUPPORTED; the
  software path uses wl_shm buffers (no bind needed).

## Runtime ship-set per arch (identical layout both arches; ELF verified x86-64 / aarch64)
Mesa-built (stage-<arch>/usr/lib):
  libEGL.so.1.0.0, libGLESv2.so.2.0.0, libGLESv1_CM.so.1.1.0, libgbm.so.1.0.0,
  libgallium-25.3.6.so (megadriver: softpipe+swrast+kms_swrast), gbm/dri_gbm.so,
  libexpat.so.1.8.10, libz.so.1.3.1   (last two are mesa subprojects)
External deps to ship (sysroot-<arch>/usr/lib):
  libdrm.so.2.134.0, libwayland-client.so.0.23.1, libwayland-server.so.0.23.1,
  libwayland-egl.so.1.23.1, libwayland-cursor.so.0.23.1, libffi.so.8.1.4
  + musl libc.so + ld-musl-<arch>.so.1 loader (NOT relibc) — every lib NEEDs libc.so.

## NEEDED-graph delta vs S3 surfaceless (both arches)
  libEGL:   +libwayland-client.so.0  +libwayland-server.so.0   (was: gallium,expat,gbm,drm,libc)
  dri_gbm:  +libwayland-server.so.0  (pulled by the bind-wayland-display / wl_drm server path)
  new transitive: libwayland-client/server NEED libffi.so.8; libwayland-cursor NEEDs libwayland-client.
  (If built WITHOUT -Dlegacy-wayland=bind-wayland-display, libEGL keeps only libwayland-client.so.0 and
   dri_gbm drops libwayland-server.)

## VERDICT
Goal 1 (wayland x86_64): DONE. EGL_EXT_platform_wayland present. EGL_WL_bind_wayland_display double-gated
  off for swrast (compile-opt + runtime dma-buf) — documented above.
Goal 2 (aarch64): DONE. Toolchain (zig cc -target aarch64-linux-musl) + lld produced clean aarch64 ELF for
  libdrm, libffi, libwayland, and the whole Mesa tree. No aarch64-specific zig/lld blocker encountered.
