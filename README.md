# LeandrOS

A `no_std` microkernel written in Rust, targeting **x86-64** and **AArch64** (QEMU, Raspberry Pi 5, and PC hardware).

The core provides scheduling, IPC, and memory management; drivers, filesystems, and network stacks are structured as separate server crates reached through IPC ports. On top of that core sits a **Linux-compatible syscall personality** — the real Linux syscall numbers on each architecture — so unmodified `*-unknown-linux-musl` binaries run without source patches: a POSIX shell, coreutils, Mesa, D-Bus, Firefox, and a full **COSMIC Wayland desktop** behind a graphical login, rendered on the host GPU through virtio-gpu (Zink over Vulkan/Venus, or virgl).

## Quick start

```sh
./scripts/build-all.sh             # release builds, both architectures
./scripts/run-qemu.sh aarch64
./scripts/run-qemu.sh x86_64
```

Log in as `root`/`root` or `leandro`/`leandro`. See [Getting started](docs/getting-started/README.md) for prerequisites and options.

## Documentation

Everything else — architecture, the design decisions behind each part of the kernel and the OS, hardware support, and the development workflow — is in the **[LeandrOS Handbook](docs/README.md)**.

## License

GPL-3.0
