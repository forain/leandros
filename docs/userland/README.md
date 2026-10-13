# Userland

Everything that runs at EL0 / ring 3 above the server layer: PID 1, the login programs,
the shells, utilities, regression tests, and the third-party software that makes up the
desktop. Because the kernel implements the real Linux syscall numbers, most of this is
ordinary Linux software built for `*-unknown-linux-musl`; a smaller in-tree set is written
against LeandrOS's own minimal C runtime.

## Overview

LeandrOS userland comes from three places and is linked three ways.

| Source | Examples | Linked against | Built by |
|---|---|---|---|
| In-tree `no_std` programs ([userland/](../../userland/)) | `init`, `login`, `greeter-launch`, `mount`, `lsblk`, `aplay`, `vfstest` | `leandros-libc`, static `ET_EXEC` on `*-unknown-none` | [scripts/build-userland.sh](../../scripts/build-userland.sh) |
| In-tree tests that need a full libc | `pthreadtest`, `sigtest`, `polltest`, `drmsmoke`, `vttest`, `ping` | relibc (`librelibc.a`), custom `*-unknown-leandros` target | the same script, with `-Z build-std` |
| In-tree `std` crates and ports | `killmt`, `spawnwedge`, `ports/sysbus`, `ports/mkfs-xfs`, greetd, busd | musl, static (`-C relocation-model=static`) | each crate's own `build.sh` |
| Sibling checkouts | brush, uutils coreutils, bottom, disks-rs | musl, static, through `zig cc` | `scripts/build-all.sh` |
| Prebuilt or cross-built third-party | COSMIC, Mesa, libinput, Firefox, PipeWire, the portal | musl, **dynamic** against the shipped `ld-musl` | `ports/*` recipes, the artifacts tree, Alpine containers |

All of it lands in one f2fs root image, packed by
[scripts/mkfs-f2fs-populated.py](../../scripts/mkfs-f2fs-populated.py), which is also where
accounts, `/etc` files, icon themes and desktop entries are seeded.

| Page | What it covers |
|---|---|
| [libc.md](libc.md) | relibc, `leandros-libc`, musl (static and dynamic), and how pthreads are layered between kernel and libc |
| [programs.md](programs.md) | every program in the image: init, login, the shells, utilities, coreutils, and where the tests are documented |
| [ports.md](ports.md) | the `ports/` directory — one entry per port with its recipe and design notes — plus Firefox, brush, DOOM, MAME and disks-rs |

## Design decisions

- **Linux binaries, unmodified, as the default.** The goal for third-party code is that
  nothing in it knows it runs on LeandrOS. That is why the musl path is first-class rather
  than a compatibility layer: it is how the desktop runs, and it works precisely because the
  kernel implements Linux's syscall numbers and semantics rather than a translation layer.
  When a port needs a change, the change goes into LeandrOS first, a build flag second, and a
  source patch to the third-party project last — and never, by policy, into COSMIC (see
  [../desktop/README.md](../desktop/README.md#zero-cosmic-source-patches-with-one-approved-exception)).
- **In-tree code is Rust by rule.** Programs, tests and probes written for LeandrOS are Rust
  crates under `userland/` or `ports/`; a capability worth keeping is rewritten as a crate
  rather than imported as C.
  Third-party C (Mesa, PipeWire, Firefox's closure) is built or repackaged by recipes, not
  vendored. The C that is tracked sits where a C ABI or a C build forces it: the libseat and
  libudev shims ([ports/input-stack/shims](../../ports/input-stack/shims/)), small programs
  compiled inside Alpine containers against C libraries (`gpuprobe`, the `__stack_chk_guard`
  shim, the PipeWire sink and ScreenCast probe, a Firefox icon tracer), the DOOM platform
  layer, the NVIDIA OpenRM glue, and four tests — `userland/privtest/privtest.c`,
  `userland/nettest/nettest.c` and `userland/syscalltest/syscalltest.c`, each built for both
  the guest and a Linux host so the two outputs can be diffed with Linux as the oracle, and
  `userland/libc/src/test_mmap.c`.
- **Static where it is cheap, dynamic where `dlopen` requires it.** Small in-tree programs and
  most ports are static, so they depend on nothing in the image. The graphics and desktop
  stack is dynamic because `dlopen` is on its critical path ([libc.md](libc.md#musl)).
- **Tests live with the userland.** Every kernel fix that a program exposed gets a regression
  case in a `userland/*test` crate, run in QEMU on both architectures; see
  [../development/testing.md](../development/testing.md).

## See also

- [../desktop/README.md](../desktop/README.md) — the COSMIC desktop built from these pieces
- [../kernel/syscalls.md](../kernel/syscalls.md) — the Linux syscall personality the userland relies on
- [../kernel/processes.md](../kernel/processes.md) — `execve`, the ELF loader, `PT_INTERP`
- [../getting-started/building.md](../getting-started/building.md) — building the image
- [../overview/workspace-layout.md](../overview/workspace-layout.md) — sibling repositories
