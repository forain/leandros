# lane/nvkbuild - NVK (Mesa nouveau Vulkan) in the GPU stack, 2026-10-07

Status: built and booted on both arches (see "Result"); Venus half unverified.
The sections "What changed" and "Why NVK cannot be picked" describe the first
(unexecuted) pass; "Result" supersedes its version numbers (llvm 19, not 20).

## What changed

- `ports/mesa/build-gpu-stack.sh` (host side)
  - `NVK=1` is the default (`NVK=0` restores the old virtio-only build).
  - Downloads every crates.io wrap in the Mesa tree (`subprojects/*-rs.wrap`)
    into `$ART/m3-gl-stack/nvk-cache/crates/<source_filename>` and verifies
    each against the wrap's `source_hash` (sha256). Mismatch aborts.
  - Mounts the cache at `/cache` in the container.
- `ports/mesa/build-gpu-stack-alpine.sh` (container side)
  - `-Dvulkan-drivers=virtio,nouveau`; gallium drivers unchanged
    (zink,virgl,softpipe[,v3d]; zink was already present).
  - Still `--wrap-mode=nodownload`: crate tarballs are copied from `/cache`
    into the private Mesa copy's `subprojects/packagecache`.
  - Two-stage build because NVK needs `mesa_clc` + `vtn_bindgen2`, which need
    LLVM/libclc/SPIRV-LLVM-Translator:
    stage 1 = driver-less tree with `-Dllvm=enabled -Dmesa-clc=enabled
    -Dinstall-mesa-clc=true`, installed to `/tmp/clc-inst`;
    stage 2 = the real tree with `-Dllvm=disabled -Dmesa-clc=system`.
    So no LLVM is linked into any shipped ELF (the "no LLVM" property of the
    existing stack is preserved).
  - Toolchain: Alpine `rust cargo` (3.21: 1.83, Mesa needs >= 1.82),
    `clang20-dev clang20-libclang llvm20-dev libclc spirv-llvm-translator-dev
    spirv-tools-dev`; Meson 1.8.2 from pip (Mesa Rust needs >= 1.7, Alpine 3.21
    has 1.6); bindgen-cli 0.72.1 via `cargo install --locked` (Mesa needs
    >= 0.71.1, Alpine 3.21 has 0.70), cached in
    `/cache/bindgen-0.72.1-<arch>/` so it is built once per arch. The only
    network use beyond apk is that one `cargo install` (pinned, `--locked`).
  - Output checks: `libvulkan_nouveau.so` is added to the NEEDED listing, the
    "musl soname still present" check, and the nouveau ICD JSON is printed.
- `scripts/mkfs-f2fs-populated.py`: stages `libvulkan_nouveau.so` and
  `nouveau_icd.<arch>.json` into `/usr/lib` and `/usr/share/vulkan/icd.d`
  when the stage has them (optional; absent stage = unchanged image).

## Why NVK cannot be picked in a QEMU run (read from the tree, not yet booted)

`ports/greetd/data/gpu-env` selects the Vulkan ICD explicitly: it globs only
`/usr/share/vulkan/icd.d/virtio_icd.*.json` and exports
`VK_ICD_FILENAMES=<that file>` before probing zink. The Khronos loader then
never opens the nouveau manifest in the COSMIC session. Tools that run without
gpu-env (plain `vulkaninfo`, `vktest`) would see both ICDs; NVK probes DRM
nodes, finds no nouveau device and should report zero physical devices.
That is the part still to confirm by booting.

## Crate versions (Mesa 25.3.6 wraps, all sha256-verified, 27 tarballs, 2.6 MB)

bitflags 2.9.1, cfg-if 1.0.0, equivalent 1.0.1, errno 0.3.12, hashbrown
0.14.1, indexmap 2.2.6, libc 0.2.168, log 0.4.27, once_cell 1.8.0, paste
1.0.14, pest/pest_derive/pest_generator/pest_meta 2.8.0, proc-macro2 1.0.86,
quote 1.0.35, remain 0.2.12, roxmltree 0.20.0, rustc-hash 2.1.1, rustix 1.0.7,
syn 2.0.87, thiserror/thiserror-impl 2.0.11, ucd-trie 0.1.6, unicode-ident
1.0.12, zerocopy/zerocopy-derive 0.8.13. (Only paste, rustc-hash, syn, quote,
proc-macro2, unicode-ident and friends are used by NAK; the rest are for
rusticl/etnaviv and are harmless.)

## Result (second run, 2026-10-07): BUILT and booted on both arches

Docker Desktop (aarch64 VM, 16 CPUs) now works. x86_64 runs as a qemu-user
container on this aarch64 host (Rosetta off); aarch64 is native.

Recipe fixes the real build forced (all in the alpine/host scripts):

- Alpine 3.21 has llvm/clang **19** (not 20): `clang19-dev clang19-libclang
  llvm19-dev`, `LIBCLANG_PATH=/usr/lib/llvm19/lib`. `libclc` alone has no .pc;
  `libclc-dev` is needed. libclc 19.1.4 and spirv-llvm-translator match llvm19.
- `cbindgen` (NIL) was missing: added to the apk list.
- Stage 2 with `-Dmesa-clc=system` makes Mesa auto-link SPIRV-Tools (debug
  dumps only); Alpine's is shared-only, so `libSPIRV-Tools.so` landed in the
  NEEDED of libgallium AND both ICDs (unshipped => would have broken zink).
  Fixed with `-Dspirv-tools=disabled` in stage 2, and a new post-build guard
  fails the build (rc=6) if libLLVM/libclang/libSPIRV leak into NEEDED.
  Stage 1 options (`-Dgallium-drivers=[] -Dvulkan-drivers=[] -Dplatforms=[]
  -Dllvm=enabled -Dmesa-clc=enabled -Dinstall-mesa-clc=true ...`) worked as
  written, nothing else needed.
- Under qemu-user Alpine's rustc/cargo abort at startup ("Scudo ERROR: CHECK
  failed @ primary32.h"). They only link libscudo for malloc; the alpine
  script now replaces /usr/lib/libscudo.so with an empty stub when
  `rustc --version` fails (self-detecting; native containers skip it).
- Rust staticlib link (-static-libgcc + ssp_guard.o) needed no change.
- `GPU_BUILD_IMAGE=<image>` env on the host script overrides alpine:3.21 so a
  pre-baked image (apk + pip layers) can be used; a fresh alpine pull spends
  ~10-15 min in apk on this network before any compile starts.

Build times (stage 1 mesa_clc + stage 2 full tree, from a pre-baked image,
bindgen already cached): aarch64 native 237 s; x86_64 under qemu-user
1503 s (25 min, includes the one-time `cargo install bindgen-cli`).
First aarch64 run from plain alpine:3.21 incl. apk + bindgen compile: ~26 min.

Versions: Alpine 3.21, rustc/cargo 1.83.0, meson 1.8.2 (pip), bindgen-cli
0.72.1, llvm/clang 19.1.4 (stage 1 only), libclc 19.1.4, Mesa 25.3.6.

Sizes: libvulkan_nouveau.so aarch64 16,986,465 B, x86_64 17,474,281 B (ICD
manifest api_version 1.4.328). libgallium-25.3.6.so and libvulkan_virtio.so are
byte-size identical to the pre-NVK stage (25,116,817/1,904,593 aarch64;
25,284,617/1,875,969 x86_64). Stage dirs 32 MB -> 48 MB each; nvk-cache 19 MB.

NEEDED of libvulkan_nouveau.so (both arches, no LLVM/SPIRV/libstdc++):
libdrm.so.2 libz.so.1 libzstd.so.1 libwayland-client.so.0 libdisplay-info.so.3
libexpat.so.1 libc.so. Every other ELF's NEEDED list is unchanged.

## Boot evidence (Mac: aarch64/HVF and x86_64/TCG, virgl path)

Images rebuilt with scripts/mkfs-f2fs-populated.py (run with the main
checkout's built userland as cwd; kernel/initrd/limine images APFS-cloned).
`/usr/lib/libvulkan_nouveau.so` and `/usr/share/vulkan/icd.d/nouveau_icd.<arch>.json`
are present next to virtio_icd.<arch>.json in the guest.

- ffsession `--nofirefox` on both arches: greeter, cosmic-panel, cosmic-term
  all came up; screenshots show panel, Orion wallpaper, terminal. aarch64 gpu-env
  chose virgl (the Mac QEMU has no Venus; see below).
- runtests `--suite regress` on both arches: all 15 suites RC=0, overall PASS.
- No `vulkaninfo` exists in the guest, so a 30-line loader client
  (`vkCreateInstance` + `vkEnumeratePhysicalDevices`, linked to the staged
  libvulkan.so.1) was built in the container and staged via
  LEANDROS_EXTRA_BIN for the check only (not committed). Plain and
  ICD-pinned runs, both arches: loader dlopens libvulkan_nouveau.so (the
  MMAP-BIG line shows the 15-23 MiB mapping), vkCreateInstance succeeds,
  vkEnumeratePhysicalDevices returns rc=-3 (VK_ERROR_INITIALIZATION_FAILED),
  0 devices, no crash, no hang, desktop unaffected.

NOT verified: Venus. The Mac QEMU builds have no `venus=` property
(`~/.local/qemu-gpu-venus` has no qemu-system binaries) and the linux desktop
was unreachable (ssh timeout), so "NVK reports nothing AND Venus is still
listed" and "COSMIC on Zink/Venus" remain to be confirmed on a Linux/KVM
host with `run-qemu.sh --venus`. The virtio ICD and libgallium are
byte-identical in size and NEEDED to before, and gpu-env still pins
VK_ICD_FILENAMES to the virtio manifest on the Venus path, so no change is
expected.

## Build / update

    ports/mesa/build-gpu-stack.sh x86_64      # qemu-user on an aarch64 Mac: ~25 min
    ports/mesa/build-gpu-stack.sh aarch64
    NVK=0 ports/mesa/build-gpu-stack.sh x86_64   # old behaviour

Updating Mesa: point `MESA_SRC` at the new tree; the host script re-reads its
`subprojects/*-rs.wrap` and fetches/verifies whatever changed (old tarballs in
`nvk-cache/crates` are harmless). Updating bindgen: bump `BINDGEN_VER` in the
alpine script (a new cache directory is built on first use).
Test an image without replacing the shared stage:
`LEANDROS_GPU_STAGE=<dir> ./scripts/mkfs-f2fs-populated.py ...` (see script).
