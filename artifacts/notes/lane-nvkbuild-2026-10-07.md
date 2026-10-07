# lane/nvkbuild - NVK (Mesa nouveau Vulkan) in the GPU stack, 2026-10-07

Status: build recipe written and the crate cache verified; the container build
itself has NOT been run (see "Blocked"). Nothing below under "Unverified" has
been observed.

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

## Blocked

On the Mac, Docker Desktop answers every command with "Sign in to continue
using Docker Desktop. Sign in enforced by your administrators", so no
container can run; podman/colima are not installed. The linux desktop
(172.16.158.150 and its Tailscale 100.106.210.103) and the laptop
(172.16.149.179) both timed out on ssh. So: no build, no sizes, no readelf, no
QEMU boots yet.

## Unverified guesses that the first real build may trip over

- Alpine 3.21 package names/versions (`clang20-libclang`, `libclc`,
  `spirv-llvm-translator-dev`; llvm20 vs libclc version match).
- `-Dgallium-drivers=[] -Dvulkan-drivers=[] -Dplatforms=[]` in the mesa_clc
  stage may need extra `-D...=disabled` options in 25.3.6.
- Rust staticlib link into `libvulkan_nouveau.so` with `-static-libgcc` and the
  `ssp_guard.o` link args; if `libc.musl-<arch>.so.1` NEEDED shows up it is
  rewritten by the existing patchelf loop.
- If libLLVM leaks into the NEEDED list of the ICD, `-Dmesa-clc=system` was not
  honoured.

## Build / update

    ports/mesa/build-gpu-stack.sh x86_64      # on a machine with podman/docker
    ports/mesa/build-gpu-stack.sh aarch64
    NVK=0 ports/mesa/build-gpu-stack.sh x86_64   # old behaviour

Updating Mesa: point `MESA_SRC` at the new tree; the host script re-reads its
`subprojects/*-rs.wrap` and fetches/verifies whatever changed (old tarballs in
`nvk-cache/crates` are harmless). Updating bindgen: bump `BINDGEN_VER` in the
alpine script (a new cache directory is built on first use).
Test an image without replacing the shared stage:
`LEANDROS_GPU_STAGE=<dir> ./scripts/mkfs-f2fs-populated.py ...` (see script).
