# llvmpipe-lane progress

Host-only lane: build Mesa with llvmpipe (gallium LLVM-JIT SW rasterizer) for musl, both arches.
Repo is READ-ONLY. Outputs -> ~/code/leandros-artifacts/llvmpipe-lane/.

## Step log

### Step 0 — env probe (2026-07-23)
- Created llvmpipe-lane/ + notes/.
- m3 baseline present: ~/code/leandros-artifacts/m3-gl-stack/ (sysroot-<arch>/, out/, toolchain/, NOTES.md).
- Old Mesa checkout PRESENT: ~/.claude-forain/jobs/afde2e74/tmp/mesa-wave2/src/mesa (Mesa 25.3.6). Reuse candidate.
- repo ports/mesa/ + ports/gl-stack/ present (read-only reference).
- Docker daemon was NOT running; `open -a Docker` started it. Now UP: 29.6.2 Docker Desktop. Unblocked.
- Host LLVM 22.1.5 is macOS/arm64 (llvm-config present) — NOT usable for musl target; confirms need for target-arch LLVM (via Alpine).
- Existing Mesa recipe fully understood (ports/mesa/NOTES.md, m3 NOTES.md): Mesa 25.3.6, zig cc -target, softpipe only, `-Dllvm=disabled`. Ship set sonames: libEGL.so.1 libGLESv2.so.2 libgbm.so.1 libgallium-25.3.6.so gbm/dri_gbm.so + libexpat/libz. Every lib DT_NEEDED `libc.so` (our vanilla musl soname).
- KEY compat question to resolve: Alpine musl soname is `libc.musl-<arch>.so.1`, ours is `libc.so`. Alpine-built Mesa libs will DT_NEEDED the Alpine soname -> mismatch (fixable via patchelf or target symlink). Verifying next.

### Step 1 — DECISION: Strategy 1(b) — build Mesa 25.3.6 INSIDE Alpine per arch (native clang/llvm-dev), sidestep zig entirely. Same source + `-Dgallium-drivers=llvmpipe,softpipe -Dllvm=enabled` => identical sonames (drop-in). Then patch/verify DT_NEEDED closure vs m3 sysroot + stage new deps (libLLVM/libstdc++/libgcc_s/...).

### Step 2 — env verified for build
- Alpine 3.21: Mesa pkg=24.2.8 (rejected as version-mismatched shortcut), llvm19-dev+llvm19-static present, all Mesa build deps present. LLVM 19 within Mesa 25.3.6 support range.
- Copied Mesa 25.3.6 source (325M) to lane/src/mesa (stable, VERSION=25.3.6 confirmed). `llvmpipe` valid gallium choice.
- Wrote lane/build-in-alpine.sh: builds in container fs /tmp (fast), copies stage to /out bind mount. Config matches existing ship set + adds llvmpipe,softpipe & shared LLVM. Emits explicit `=== rc=N ===` trailer.

### Step 3 — LAUNCHED aarch64 build (native arm64 container, bg id b4n5vhrgo), log lane/logs/build-aarch64.log.

### Step 4 — RECIPE VALIDATED (aarch64 meson configure rc-clean, ninja running 1142 targets)
Config summary confirms:
- LLVM Required=enabled, Version=19.1.4 (llvm-config /usr/lib/llvm19/bin, shared-llvm=enabled)
- Gallium Drivers = **llvmpipe softpipe**; Frontends=mesa
- EGL Enabled=YES, Drivers builtin:egl_dri2/egl_dri3, **Platforms = wayland surfaceless drm** (surfaceless auto-added -> headless smoke path available WITHOUT needing /dev/dri)
- GBM enabled (internal), GLES2 YES, ES1 YES, OpenGL YES, GLVND=NO (matches existing non-glvnd stack)
- Vulkan NO, glx disabled, tools []
- Alpine dep versions in build: libdrm 2.4.123 (older than our m3 2.4.134 -> forward-compat OK), wayland 1.23.1 (== ours), expat 2.8.2, zlib 1.3.2, zstd 1.5.6, libstdc++/libgcc 14.2.0, LLVM 19.1.4.
Next: await ninja completion (task b4n5vhrgo), then verify ELF + DT_NEEDED closure + stage new deps, then smoke, then launch x86_64 (emulated, slow).

### Step 5 — aarch64 BUILD DONE (rc=0) + CLOSURE CLOSED
Ship set in stage-aarch64/usr/lib (ET_DYN, sonames identical to existing set): libEGL.so.1.0.0, libGLESv2.so.2.0.0, libGLESv1_CM.so.1.1.0, libgbm.so.1.0.0, libgallium-25.3.6.so (21.9MB, llvmpipe+softpipe, shared LLVM), gbm/dri_gbm.so.
DT_NEEDED closure verified (lane/logs/verify-aarch64.log). Post-patch union all resolvable:
  from m3 ship set: libdrm.so.2 libexpat.so.1 libffi.so.8 libwayland-client.so.0 libwayland-server.so.0 libz.so.1 libc.so
  NEW (staged in lane/deps-aarch64/): **libLLVM.so.19.1 (148MB), libstdc++.so.6, libgcc_s.so.1, libzstd.so.1, libxml2.so.2, liblzma.so.5**
libLLVM chain: libLLVM -> libffi,libz,libzstd,libxml2,libstdc++,libgcc_s; libxml2 -> libz,liblzma. All closed.
- musl soname fix APPLIED via patchelf: `libc.musl-aarch64.so.1` -> `libc.so` on all 6 Mesa libs + all 6 staged deps (Alpine musl 1.2.5 == our vanilla musl, ABI-compatible, no symbol versioning). Idempotent.
- Scripts: lane/build-in-alpine.sh, lane/verify-in-alpine.sh. Logs in lane/logs/.
Next: headless smoke (EGL surfaceless + llvmpipe, glClear+readpixels), then x86_64 emulated build.

### Step 6 — aarch64 SMOKE: llvmpipe RENDERS (PROVEN)
- EGL-surfaceless + GLES2 FBO clear+readback smoke (lane/smoke.c, lane/smoke_matrix.c).
- FINAL RESULT on the exact PATCHED target artifacts: GL_RENDERER=`llvmpipe (LLVM 19.1.4, 128 bits)`, green pixel readback, PASS on ES2/ES3/no-config/desktop-GL. softpipe path also PASS (backward compat kept).
- Debugging detour (resolved): initial smoke hit eglCreateContext BAD_ALLOC (NO_MEMORY from driCreateContextAttribs). Was NOT the artifact — it was my harness's broken libc.so symlink creating a SECOND musl instance (basic init works, llvmpipe thread/JIT startup fails => classic two-libc symptom). Controls: stock Alpine mesa 24.2.8 AND edge 25.x (LLVM22) both pass surfaceless here; our UNPATCHED libs pass; our PATCHED libs pass once libc.so is an absolute symlink to the SAME musl file the loader uses.
- **ON-TARGET LESSON (critical): patchelf libc.musl-<arch>.so.1 -> libc.so is sound ONLY IF /usr/lib/libc.so and the loader /lib/ld-musl-<arch>.so.1 are the SAME musl instance (m3 musl-dynamic guarantees this; existing softpipe stack already depends on it). A distinct/second libc.so => threaded llvmpipe context create fails NO_MEMORY while init deceptively succeeds.**

### Step 7 — LAUNCHED x86_64 build (EMULATED amd64 container, bg id byejkceti, log lane/logs/build-x86_64.log). Long pole under qemu-user. Same proven recipe. Awaiting completion.

### Step 8 — aarch64 fully DONE. Deliverable NOTES.md written (lane/NOTES.md): verdict WORKS, recipe, closure, 6 new deps (+147MB/arch), musl-instance gate, smoke results, on-target checklist + JIT/W^X kernel-surface risk. Awaiting x86_64 build (byejkceti) + milestone waiter (bo3sbmrzz).

### Step 9 — RE-ATTACH (sleep-based bg waiters get killed on this host). x86_64 container a3bc9b5ab518 confirmed Up + alive (17 build procs, ~550/1150 objs, log advancing). Build was NEVER at risk — only the completion notification was. Re-waiting via `docker wait a3bc9b5ab518` (real blocking I/O on container exit, bg id bkpse9rim). On exit: grep log for `=== rc=` (container always exits 0 since script tail-echoes; success = rc trailer in log), then verify+stage+smoke x86_64 same as aarch64.

### Step 10 — x86_64 DONE (rc=0, docker wait). Closure verified (logs/verify-x86_64.log) — IDENTICAL to aarch64, same 6 new deps staged + libc-patched. Smoke PASS (logs/smoke-x86_64.log): GL_RENDERER=`llvmpipe (LLVM 19.1.4, 256 bits)` AVX2, all context paths, under qemu-user emulation (JIT-compiled+ran x86_64 code = exercised PROT_EXEC path). Host ELF check: both arches ET_DYN, correct Machine, libc.so NEEDED. Sizes: aarch64 stage 25M/deps 147M; x86_64 stage 26M/deps 166M. NOTES.md finalized. Fixed smoke-in-alpine.sh libc symlink to single-instance absolute form.

## ============ FINAL SUMMARY ============
VERDICT: **WORKS** both arches. Mesa 25.3.6 + llvmpipe (LLVM 19.1.4 shared JIT), true drop-in (identical sonames, non-glvnd) over the softpipe-only ship set. Built native-in-Alpine (Strategy 1b), sidestepping every zig landmine.
ARTIFACTS (~/code/leandros-artifacts/llvmpipe-lane/):
  stage-<arch>/usr/lib/  = ship-set swap-in (libEGL/GLESv2/GLESv1_CM/gbm/gallium/dri_gbm, +headers)
  deps-<arch>/           = 6 NEW runtime libs: libLLVM.so.19.1(141-160M), libstdc++.so.6, libgcc_s.so.1, libzstd.so.1, libxml2.so.2, liblzma.so.5
  build-in-alpine.sh, verify-in-alpine.sh, smoke-in-alpine.sh, smoke.c, smoke_matrix.c, NOTES.md, logs/
NEW RUNTIME DEPS net +147M aarch64 / +166M x86_64 (libLLVM dominates; rest of closure already in m3 ship set).
ON-TARGET (tree wave, NOT done here): swap ship set; add 6 deps; **ensure /usr/lib/libc.so is the SAME musl instance as /lib/ld-musl (two-libc trap => NO_MEMORY)**; select via GALLIUM_DRIVER=llvmpipe; budget image size.
KERNEL PRE-CHECK: llvmpipe JITs code at runtime — kernel must allow anon mmap RW then mprotect->RX (or mmap RWX). If strict W^X, llvmpipe faults on context/first-draw. (JIT path was exercised under qemu-user in the x86_64 smoke and worked.)

