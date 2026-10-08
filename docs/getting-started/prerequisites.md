# Prerequisites

What a host needs before `./scripts/build-all.sh` can produce a bootable image and `./scripts/run-qemu.sh` can run it. Both macOS (Apple Silicon) and Linux hosts build and boot the full system for both architectures.

## Rust toolchain

LeandrOS needs a Rust **nightly** toolchain with bare-metal and musl cross targets. [`rust-toolchain.toml`](../../rust-toolchain.toml) pins the exact channel (`nightly-2026-04-16`) with `rust-src`, `rustfmt` and `clippy`, and the targets `{x86_64,aarch64}-unknown-none` and `{x86_64,aarch64}-unknown-linux-musl`; rustup fetches all of it on the first build.

```sh
rustup show   # confirms the pinned toolchain is active inside the tree
```

Beyond the pin:

- **`llvm-tools`** for the pinned toolchain (`rustup component add llvm-tools --toolchain nightly-2026-04-16`). Without `llvm-objcopy`/`rust-objcopy` the build skips — and deletes any stale copy of — the direct-boot flat binary and 32-bit ELF that Raspberry Pi deployment and `--direct` x86-64 boots use.
- **A bare, undated `nightly`** with both musl targets and `rust-src`. Some port builds (for example `ports/busd/build.sh`) call `cargo +nightly`; a missing target there fails quietly in `build-all.sh` and is only caught later, when the image builder refuses stale D-Bus staging.
- **relibc's own pin** (`nightly-2025-11-15`, from `../relibc/rust-toolchain.toml`), which rustup installs when relibc is built.

The sibling repositories (brush, coreutils, …) are built from their own directories, which `rust-toolchain.toml` does not reach; `build-all.sh` passes the pinned toolchain to them explicitly so every crate compiles with one compiler version.

## QEMU and UEFI firmware

### x86-64

```sh
# Debian / Ubuntu
sudo apt install qemu-system-x86 ovmf dosfstools mtools

# Arch Linux
sudo pacman -S qemu-system-x86 edk2-ovmf dosfstools mtools

# Fedora
sudo dnf install qemu-system-x86 edk2-ovmf dosfstools mtools
```

### AArch64

```sh
sudo apt install qemu-system-arm     # Debian/Ubuntu
sudo pacman -S qemu-system-aarch64   # Arch
```

`run-qemu.sh` searches the usual firmware locations for each distribution (OVMF/AAVMF, `edk2-ovmf`, `edk2-armvirt`, Arch's split 4 MB `CODE`/`VARS` pairs, Homebrew's `edk2-*-code.fd`) and copies a writable `VARS` template next to the image on first boot.

QEMU **11.1 or newer** is assumed on macOS: HVF only launches on a GICv3 `virt` machine since 11.1, which is what the runner configures (the kernel drives GICv2 or GICv3 by detection, so the same command line works under TCG and KVM).

### A GPU-capable QEMU

COSMIC renders on the host GPU only (see [Running](running.md#gpu-paths)), so for a desktop the host QEMU must have **virglrenderer**:

- **Linux**: a distribution QEMU built with virglrenderer and a render node (`/dev/dri/renderD*`). For Venus (Vulkan) the QEMU must expose the `venus=` device property and the host needs a Vulkan driver. Add yourself to the `kvm` group for KVM.
- **macOS**: Homebrew's QEMU has no virglrenderer and no `*-gl` devices. Build the GPU QEMU once with [`scripts/mac-qemu-gpu/build.sh`](../../scripts/mac-qemu-gpu/build.sh) (QEMU 11.1.1 + HVF + virglrenderer on ANGLE, about ten minutes, Command Line Tools only, no Xcode). The recommended variant is `--angle-vulkan`, which runs ANGLE on Vulkan/MoltenVK and gives the guest **GLES 3.1** with storage buffers, so iced/wgpu COSMIC clients work; it needs `brew install molten-vk vulkan-loader` at run time. The default variant (ANGLE on Metal) stops at GLES 3.0, on which wgpu clients cannot start. The runner picks either up automatically.

Without a GPU-capable QEMU the system still boots and the serial login works; the graphical login does not start.

## Linker

```sh
sudo apt install lld    # ld.lld is used for both targets
```

## zig and the musl cross toolchain

Third-party userspace (Mesa, COSMIC, brush, coreutils, relibc's C parts) is built for `{x86_64,aarch64}-unknown-linux-musl` and dynamically linked against a real `ld-musl`. The wrappers in `scripts/` drive that cross-build through **zig** (0.16.0 is the version in use):

- `scripts/cc-<arch>-musl.sh` — `zig cc -target <arch>-linux-musl`, stripping the `--target=` spelling that `cc-rs` appends and zig rejects.
- `scripts/linker-<arch>-musl.sh` — the Rust linker wrapper for the musl targets.
- `scripts/ar-musl.sh` — needed because `cc-rs` otherwise uses the host macOS `ar`, whose Mach-O archives `ld.lld` cannot read: the objects compile and every symbol in them comes back undefined at link time.

relibc's C sources are pointed at the same wrappers, because the host compiler only works by accident on an AArch64 host (relibc passes the AArch64-only `-mno-outline-atomics`). Without zig, the input-stack shims and the C conformance tests are skipped with a warning.

## Container tooling

Firefox, the desktop portal frontend and PipeWire/WirePlumber are not compiled: their ports install Alpine's prebuilt packages inside a container of the target architecture and repackage them for the guest. A working **podman** (preferred) or **docker** is needed the first time; [`scripts/container-lib.sh`](../../scripts/container-lib.sh) probes it with a bounded timeout (`LEANDROS_CT_TIMEOUT`, default 20 s) so a hung daemon cannot stall the build. See [Building](building.md#container-staged-ports) for the fallbacks.

## macOS specifics

- **SDK selection.** A Command Line Tools update can leave the default SDK newer than the installed linker understands (`ld: tapi error: malformed file … libSystem.B.tbd`), which breaks every host link. [`scripts/darwin-sdk.sh`](../../scripts/darwin-sdk.sh), sourced by the build, picks the newest CLT SDK that can link a trivial C program when `SDKROOT` is unset. An explicit `SDKROOT` is never overridden.
- **Networking.** Optional: `socket_vmnet` (`brew install socket_vmnet`, then `sudo brew services start socket_vmnet`) gives the guest a routable address. Without it the runner falls back to user-mode networking.
- macOS ships **openrsync**, which rejects GNU rsync flags such as `--info=stats2` and prints usage while exiting 0 — a "successful" transfer that moved nothing. Use GNU rsync for syncing trees between machines.

## Sibling repositories

Some large dependencies live outside this tree, as siblings of the repository root, and are built by `build-all.sh`:

| Path | Purpose |
|---|---|
| `../relibc` | The C standard library shipped as the userspace libc |
| `../doomgeneric` | DOOM port |
| `../cosmic-epoch` | COSMIC desktop sources (built unmodified) |
| `../brush` | The `brush` shell, at the pinned revision plus `ports/brush/patches` (`ports/brush/sync.sh apply ../brush`) |
| `../crossterm` | The crossterm fork brush depends on |
| `../coreutils` | uutils coreutils |
| `../mame`, `../bottom-leandros`, `../disks-rs` | MAME, the `bottom` monitor, AerynOS's disks-rs |

The full table, and the out-of-tree staging directory the image builder reads, are in [workspace layout](../overview/workspace-layout.md#sibling-repositories).

## See also

- [Building](building.md)
- [Running](running.md)
- [Development workflow](../development/workflow.md) — machine roles and per-machine setup.
