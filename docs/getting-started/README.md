# Getting started

How to go from a fresh checkout to a LeandrOS desktop in QEMU: install the toolchain, build the images, boot them, and log in.

The short version, from the repository root:

```sh
./scripts/build-all.sh               # release build, both architectures
./scripts/run-qemu.sh aarch64        # boot AArch64
./scripts/run-qemu.sh x86_64         # boot x86-64
```

Both architectures boot to a graphical login when the host offers a GPU path, and always offer a serial `login:` prompt on the terminal you launched from. Log in as `root`/`root` or `leandro`/`leandro`.

## Pages

- [Prerequisites](prerequisites.md) — the Rust toolchain, QEMU and firmware, the linker, zig and the musl cross toolchain, container tooling, and the sibling repositories.
- [Building](building.md) — what `build-all.sh` builds and stages, the release-only rule, the SoundFont, and the image-provenance pitfalls that make a boot run the wrong code.
- [Running](running.md) — `run-qemu.sh`: accelerators, boot modes, GPU paths (`--venus`, `--virgl`, `--no-gpu`), disks, networking, display and audio, and its environment variables.
- [Logging in](logging-in.md) — the graphical login, the accounts, the text-login opt-out, serial and VT logins.

## See also

- [Overview](../overview/README.md) — what you are building.
- [Development workflow](../development/workflow.md) — the scriptable driver used for testing.
- [Raspberry Pi 5](../hardware/raspberry-pi-5.md) and [PC hardware](../hardware/pc.md) — running on real machines.
