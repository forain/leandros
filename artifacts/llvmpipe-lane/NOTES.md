# llvmpipe-lane — Mesa 25.3.6 with llvmpipe for LeandrOS musl (both arches)

**Verdict: WORKS — both arches built, closure-verified, and llvmpipe-render smoke-PASSED.**
(aarch64 native; x86_64 emulated. x86_64 smoke JIT-compiled x86_64 code under qemu-user and
executed it — GL_RENDERER=`llvmpipe (LLVM 19.1.4, 256 bits)` [AVX2]; aarch64 = 128 bits [NEON].)
Drop-in upgrade of the existing softpipe-only GL ship set: same Mesa version (25.3.6),
same sonames, same non-glvnd config, adds the `llvmpipe` gallium driver (LLVM 19 JIT
software rasterizer) alongside `softpipe`. On x86_64 QEMU-TCG milestone exits this is the
M7 perf lever (llvmpipe is typically 5-20x faster than softpipe).

## Path chosen (Strategy 1b)
Build Mesa 25.3.6 from source INSIDE an Alpine 3.21 container per arch with the native
clang/gcc + `llvm19-dev` toolchain — NO zig. This sidesteps every zig cross-build landmine
(the reason the original ship set was softpipe-only: LLVM was unavailable to the zig build).
Alpine musl is vanilla 1.2.5 == LeandrOS's musl, so the produced ELFs are ABI-compatible;
the only fixup is a DT_NEEDED soname rewrite (below).
- aarch64 container = native (fast, minutes).
- x86_64 container = emulated via Docker Desktop qemu-user (slow; background it).

## Reproduce
```
# 0. Docker Desktop running. Mesa 25.3.6 source at ./src/mesa (VERSION == 25.3.6).
# 1. BUILD (per arch). aarch64 native; x86_64 emulated (background, ~long).
docker run --rm --platform linux/arm64 \
  -v $PWD/src/mesa:/work/mesa:ro -v $PWD:/out \
  alpine:3.21 sh /out/build-in-alpine.sh aarch64      # -> stage-aarch64/
docker run --rm --platform linux/amd64 \
  -v $PWD/src/mesa:/work/mesa:ro -v $PWD:/out \
  alpine:3.21 sh /out/build-in-alpine.sh x86_64       # -> stage-x86_64/
# 2. VERIFY closure + stage new deps + patchelf libc soname (per arch)
docker run --rm --platform linux/arm64 -v $PWD:/out alpine:3.21 sh /out/verify-in-alpine.sh aarch64
docker run --rm --platform linux/amd64 -v $PWD:/out alpine:3.21 sh /out/verify-in-alpine.sh x86_64
# 3. SMOKE (per arch; native platform recommended)
docker run --rm --platform linux/arm64 -v $PWD:/out alpine:3.21 sh /out/smoke-in-alpine.sh aarch64
```
Scripts (this dir): `build-in-alpine.sh`, `verify-in-alpine.sh`, `smoke-in-alpine.sh`,
`smoke.c`, `smoke_matrix.c`. Logs in `logs/`.

## Meson config (build-in-alpine.sh) — matches existing ship set + adds llvmpipe
```
meson setup ... --buildtype=release --wrap-mode=nodownload \
  -Dplatforms=wayland -Dlegacy-wayland=bind-wayland-display \
  -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
  -Dglx=disabled -Dgallium-drivers=llvmpipe,softpipe -Dvulkan-drivers=[] \
  -Dllvm=enabled -Dshared-llvm=enabled -Dshared-glapi=enabled -Dglvnd=disabled \
  -Dtools=[] -Dvalgrind=disabled
```
Deltas vs the old softpipe recipe (ports/mesa/build-mesa-wayland.sh): `-Dgallium-drivers`
adds `llvmpipe`; `-Dllvm=enabled -Dshared-llvm=enabled` (was `-Dllvm=disabled`). Everything
else identical. `shared-llvm` links libLLVM dynamically (one 141 MB .so shipped once) rather
than statically bloating libgallium. Config summary confirmed: LLVM 19.1.4, Gallium drivers
= llvmpipe softpipe, EGL platforms = wayland surfaceless drm, GBM internal, GLVND NO.

## Artifacts (per arch)
`stage-<arch>/usr/lib/` — the ship-set (ET_DYN, sonames IDENTICAL to existing set):
```
libEGL.so.1.0.0            libGLESv2.so.2.0.0     libGLESv1_CM.so.1.1.0
libgbm.so.1.0.0           libgallium-25.3.6.so (~22 MB, llvmpipe+softpipe)
gbm/dri_gbm.so            (+ .so / .so.N symlinks; + usr/include headers)
```
`deps-<arch>/` — the NEW runtime libs llvmpipe drags in, extracted from Alpine apks,
soname-symlinked, and libc-patched (see below):
```
libLLVM.so.19.1  (~141 MB)   libstdc++.so.6(.0.33)   libgcc_s.so.1
libzstd.so.1(.5.6)           libxml2.so.2(.13.9)     liblzma.so.5(.8.3)
```

## DT_NEEDED closure (verified, logs/verify-aarch64.log)
Per-lib NEEDED, and the full union, all resolvable:
- `libgallium-25.3.6.so` -> libdrm.so.2, **libLLVM.so.19.1**, libexpat.so.1, libz.so.1,
  **libzstd.so.1**, **libstdc++.so.6**, **libgcc_s.so.1**, libc.so
- `libEGL.so.1` -> libgallium, libgbm.so.1, libexpat.so.1, libdrm.so.2,
  libwayland-client.so.0, libwayland-server.so.0, libc.so
- `libGLESv2.so.2`, `libGLESv1_CM.so.1` -> libgallium, libc.so
- `libgbm.so.1` -> libdrm.so.2, libexpat.so.1, libc.so
- `gbm/dri_gbm.so` -> libgallium, libwayland-server.so.0, libdrm.so.2, libexpat.so.1, libc.so
- **libLLVM.so.19.1** -> libffi.so.8, libz.so.1, **libzstd.so.1**, **libxml2.so.2**,
  libstdc++.so.6, libgcc_s.so.1, libc.so
- **libxml2.so.2** -> libz.so.1, **liblzma.so.5**, libc.so ; libstdc++ -> libgcc_s -> libc.so

Already satisfied by the existing m3 ship set: libdrm.so.2, libexpat.so.1, libffi.so.8,
libz.so.1, libwayland-client/server.so.0, libc.so.

**NEW runtime deps to add to the ship set (6, staged in deps-<arch>/):**
`libLLVM.so.19.1` (141 MB), `libstdc++.so.6`, `libgcc_s.so.1`, `libzstd.so.1`,
`libxml2.so.2`, `liblzma.so.5`. Net footprint add ≈ 147 MB/arch (libLLVM dominates; already
stripped — it is genuinely that large because Alpine builds all LLVM targets). This is a
notable initramfs/rootfs growth — flag for image-size budgeting.

## musl soname fix (applied by verify-in-alpine.sh)
Alpine links libc as soname `libc.musl-<arch>.so.1`; the LeandrOS ship set uses `libc.so`.
verify-in-alpine.sh runs `patchelf --replace-needed libc.musl-<arch>.so.1 libc.so` on all 6
Mesa libs AND all 6 staged deps. Vanilla-musl-to-vanilla-musl, no symbol versioning => sound.
**Hard requirement (proven the hard way — see smoke):** on target, `/usr/lib/libc.so` and the
loader `/lib/ld-musl-<arch>.so.1` MUST be the SAME musl instance (m3 musl-dynamic sysroot
guarantees this, and the existing softpipe stack already relies on it). If libc.so is a
distinct/second musl, EGL init deceptively succeeds but the first threaded llvmpipe context
creation fails with EGL_BAD_ALLOC (dri NO_MEMORY). Do NOT ship a second/independent libc.so.

## Smoke (host-side, verified — aarch64)
EGL-surfaceless + GLES2, create context, FBO RGBA clear, glReadPixels (smoke.c / matrix in
smoke_matrix.c). Run against the EXACT patched artifacts:
```
GL_RENDERER = llvmpipe (LLVM 19.1.4, 128 bits)
PIXEL rgba = 0,255,0,255      SMOKE PASS
```
PASS on ES2/ES3/no-config/desktop-GL; softpipe path also PASS (backward compat kept). x86_64
smoke (logs/smoke-x86_64.log): identical PASS under qemu-user emulation, 256-bit (AVX2).
Controls used to localize the earlier harness bug: stock Alpine mesa 24.2.8 and edge 25.x
(LLVM22) both pass surfaceless in the same container; our unpatched libs pass; our patched
libs pass once libc.so is a single-instance absolute symlink. Note: the on-target path is GBM
over /dev/dri/card0 (real DRM, kms_swrast) — a different, more forgiving context path than the
headless surfaceless-without-DRM path used for this host smoke.

## On-target integration checklist (for the tree-owning wave — do NOT do here)
1. Ship-set swap: replace the softpipe-only libgallium-25.3.6.so + libEGL/libGLESv2/
   libGLESv1_CM/libgbm/dri_gbm with the ones in stage-<arch>/usr/lib/ (sonames identical, so
   existing clients kmscube/anvil/cosmic-comp need no relink).
2. Add the 6 new deps from deps-<arch>/ to /usr/lib on target (with soname symlinks).
3. Confirm /usr/lib/libc.so is the SAME musl file as /lib/ld-musl-<arch>.so.1 (single
   instance). This is the one correctness gate.
4. Select llvmpipe at runtime: `GALLIUM_DRIVER=llvmpipe` (env), or a drirc default. Without
   it the megadriver still defaults to a software driver; setting it forces llvmpipe over
   softpipe. `LIBGL_ALWAYS_SOFTWARE` is not required (no HW driver exists) but harmless.
5. Image-size budget: +~147 MB/arch (libLLVM). If that is too heavy, the fallback is a
   static-LLVM build (`-Dshared-llvm=disabled` + llvm19-static) folding LLVM into libgallium
   (still large, one file) — not built here.

## Risks / flags for the tree wave
- **JIT under QEMU-TCG**: llvmpipe JITs x86_64/aarch64 machine code at runtime and executes
  it. Under TCG this JIT-then-run is exactly the workload TCG is slowest at translating, BUT
  it replaces a far larger volume of softpipe interpreted C work — expect net speedup, though
  less than the 5-20x seen on native hardware. Worth measuring on a real milestone exit.
- **W^X / PROT_EXEC JIT pages (KERNEL SURFACE TO PRE-CHECK)**: llvmpipe (LLVM MCJIT/ORC via
  libffi) allocates memory, writes generated code, and makes it executable at runtime. On
  LeandrOS this newly exercises the mmap/mprotect PROT_EXEC path in a way the softpipe stack
  never did. The kernel must allow: `mmap(PROT_READ|PROT_WRITE)` then `mprotect(...,
  PROT_READ|PROT_EXEC)` on anonymous pages (RW->RX transition), or `mmap(PROT_READ|PROT_WRITE
  |PROT_EXEC)` directly. If the kernel enforces strict W^X or rejects PROT_EXEC on anon/heap
  mappings, llvmpipe context creation or first-draw will fault. Pre-check that mprotect
  RW->RX on anonymous memory is permitted before integrating.
- LLVM 19 vs future: pinned to Alpine 3.21's llvm19 (19.1.4). If the container base moves,
  libLLVM soname changes (libLLVM.so.20.1 etc) and libgallium's NEEDED must track it.
```
```
