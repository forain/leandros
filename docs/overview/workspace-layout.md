# Workspace layout

The repository is one Cargo workspace for the kernel and its crates, a second workspace for in-tree userland programs, build recipes for third-party software under `ports/`, and a set of sibling repositories next to the checkout that the build compiles from source.

## Kernel workspace crates

The root [`Cargo.toml`](../../Cargo.toml) lists these members. The release profile uses `lto = true`, `opt-level = "s"` and `panic = "abort"`.

| Crate | Purpose | Handbook |
|---|---|---|
| [`kernel`](../../kernel) | Entry point (`kernel_main`), the syscall dispatcher and Linux personality, init-time server registration, kernel log | [kernel](../kernel/README.md), [syscalls](../kernel/syscalls.md) |
| [`mm`](../../mm) | Buddy allocator, slab allocator, VMM and VMAs, page tables, copy-on-write, page cache | [memory](../kernel/memory.md) |
| [`sched`](../../sched) | Preemptive SMP scheduler, context switch, clone/fork, futexes, signals, credentials, watchdog and lock tracking, user-copy accessors | [scheduler](../kernel/scheduler.md), [processes](../kernel/processes.md) |
| [`ipc`](../../ipc) | Port table, message types, direct port handlers, `Channel` abstraction | [IPC](../kernel/ipc.md) |
| [`capability`](../../capability) | Capability handle types passed in message slots | [IPC](../kernel/ipc.md) |
| [`elf`](../../elf) | ELF64 parser and loader: `ET_EXEC`, `ET_DYN` at a load bias, `PT_INTERP` | [processes](../kernel/processes.md) |
| [`boot`](../../boot) | Multiboot2, Limine and Device Tree (FDT) parsers producing `BootInfo` | [boot](../kernel/boot.md) |
| [`arch/x86_64`](../../arch/x86_64) | GDT/TSS, IDT, APIC/x2APIC, IOAPIC, PIC, SYSCALL entry, SMP, timer, RTC | [x86-64](../kernel/arch-x86_64.md) |
| [`arch/aarch64`](../../arch/aarch64) | MMU, exception vectors, GICv2/GICv3, generic timer, PL011 UART, SMP via PSCI | [AArch64](../kernel/arch-aarch64.md) |
| [`drivers`](../../drivers) | KMS and the DRM device layer, virtio (GPU, sound, block, net, input, rng), framebuffer console, serial, PCI, SDHCI, USB mass storage and HID glue, Raspberry Pi mailbox | [drivers](../hardware/drivers.md), [graphics](../graphics/README.md) |
| [`drivers/usb`](../../drivers/usb) | xHCI host controller | [PC hardware](../hardware/pc.md) |
| [`drivers/wifi`](../../drivers/wifi) | mac80211 and virtio-wifi | [drivers](../hardware/drivers.md) |
| [`servers/vfs`](../../servers/vfs) | Virtual file system: fd tables, open file descriptions, tmpfs, mounts, pipes, eventfd/timerfd/signalfd/inotify, block-device layer and synthetic `/sys/class/block` | [VFS](../servers/vfs.md) |
| [`servers/fuse`](../../servers/fuse) | FUSE: `/dev/fuse` (10:229), protocol 7.31, `fuse`/`fuse.<sub>` mounts | [VFS](../servers/vfs.md#other-in-vfs-filesystems) |
| [`servers/f2fs`](../../servers/f2fs) | F2FS file system — persistent storage on virtio-blk, SD or USB | [file systems](../servers/filesystems.md) |
| [`servers/xattr`](../../servers/xattr) | Shared xattr and POSIX ACL contract: codec, namespace gates, the one access evaluator | [file systems](../servers/filesystems.md), [security](../servers/security.md) |
| [`servers/drm`](../../servers/drm) | DRM server: Linux DRM ioctls, KMS, PRIME, fences | [DRM/KMS](../graphics/drm-kms.md) |
| [`servers/evdev`](../../servers/evdev) | Linux-compatible input event server (`evdev-server`; a dependency of `kernel` and `drivers` rather than a workspace member) | [input](../input.md) |
| [`servers/pipewire`](../../servers/pipewire) | Kernel audio server backing `/dev/pipewire` | [audio](../audio.md) |
| [`servers/tty`](../../servers/tty) | TTY server: line discipline, job control, PTYs, virtual terminals, POSIX timers | [tty and VT](../servers/tty-and-vt.md) |
| [`servers/net`](../../servers/net) | Network server: smoltcp TCP/IP, DHCP, ICMP, and a full AF_UNIX implementation | [networking](../servers/networking.md) |
| [`servers/libc-shim`](../../servers/libc-shim) | In-kernel glue backing the userspace libc | [libc](../userland/libc.md) |
| [`lib`](../../lib) | `align_up`/`align_down` and other utilities shared across crates | — |

Loadable-module support adds `lib/` crates, listed in the root workspace: [`lib/kmod-abi`](../../lib/kmod-abi) (the module/kernel ABI types), [`lib/kmod-elf`](../../lib/kmod-elf) (the module ELF loader and relocator), [`lib/kmod-kapi`](../../lib/kmod-kapi) (the kernel API shim modules link against) and [`lib/kmod-rt`](../../lib/kmod-rt) (the module runtime). [`lib/bt-host`](../../lib/bt-host) is the host-side Bluetooth stack shared by `userland/btd` and `userland/bthid`, and [`lib/netctl-abi`](../../lib/netctl-abi) the `/dev/netctl` ABI. Each driver built as a module is a crate under [`modules/`](../../modules) (`virtio-blk`, `nvme`, `virtio-net`, `e1000e`, `iwlwifi`, `usb-hid`, `usb-storage`, `uvc`, `bt`, `hda`, `virtio-snd`, `virtio-rng`, `virtio-keyboard`, `hello`). Many drivers also have a sibling `*-tests` host crate (for example `drivers/bt-tests`, `drivers/v3d-tests`, `drivers/brcmf-tests`) that runs their logic on the build host. See [kernel modules](../kernel/modules.md).

The sandboxing work adds two more `lib/` crates: [`lib/seccomp-bpf`](../../lib/seccomp-bpf) (the classic-BPF checker and interpreter behind seccomp) and [`lib/ebpf-cgdev`](../../lib/ebpf-cgdev) (the eBPF verifier and interpreter for cgroup device programs); see [namespaces](../kernel/namespaces.md).

[`drivers/nvidia`](../../drivers/nvidia) (`nvidia-rm`) is an optional kernel dependency behind the `nvidia` feature (x86-64 only): the OS-interface shim for NVIDIA's open GPU kernel modules ([PC hardware](../hardware/pc.md)).

## Other top-level directories

| Path | Contents |
|---|---|
| [`userland/`](../../userland) | In-tree programs (including the Bluetooth daemon `btd` and HID bridge `bthid`) and regression suites, as their own Cargo workspace ([`userland/Cargo.toml`](../../userland/Cargo.toml)); linked against `leandros-libc`, relibc or musl. A few (`spawnwedge`, `killmt`) are std/musl crates with their own `build.sh`. See [programs](../userland/programs.md) and [testing](../development/testing.md). |
| [`ports/`](../../ports) | Build recipes and integration files for third-party software: Mesa, D-Bus/busd and the system-bus services, COSMIC components, greetd and `pam-leandros`, input-stack shims (libseat, libudev), PipeWire, Firefox, the desktop portal, brush, mkfs.fat, mkfs.xfs, OpenRM. Each has its own README. See [ports](../userland/ports.md). |
| [`Kconfig`](../../Kconfig) and `drivers/Kconfig` | The kernel configuration tree (symbols map to `drv-*` cargo features); edit with `scripts/menuconfig.sh`, which uses the vendored kconfiglib in `scripts/kconfig/`. |
| [`modules/`](../../modules) | Loadable kernel module crates, built by `scripts/kmod/` (`kmod-build.sh`, `kmodpost.py`, `bidcheck.py`). |
| [`scripts/`](../../scripts) | Build, image, run and deploy scripts (`build-all.sh`, `run-qemu.sh`, `mkfs-f2fs-populated.py`, the Raspberry Pi and USB-stick writers, benchmarks). |
| [`artifacts/`](../../artifacts) | Versioned investigation instruments — session drivers, capture harnesses — and `artifacts/notes/`, the lane notes each measured claim in this handbook rests on. Authored sources, notes and harnesses only; large binaries live outside the repository. |
| [`.claude/skills/run-leandros/`](../../.claude/skills/run-leandros) | The scriptable QEMU driver (`driver.py`) and the test and session runners built on it. See [workflow](../development/workflow.md). |
| [`scripts/toolchain/`](../../scripts/toolchain) | The clang/lld musl cross toolchain: static `<arch>-linux-musl-{cc,c++}` wrappers, `mk-sysroot.sh`/`ensure.sh` over the locked Alpine sysroot (`ports/alpine/toolchain.lock`), `dyn/` for the dynamic COSMIC toolchain. |
| [`linkers/`](../../linkers) | Kernel linker scripts: Limine and direct-boot variants per architecture. |
| [`targets/`](../../targets) | Custom target specs: `*-unknown-kernel.json` for the kernel, `x86_64-unknown-leandros.json` for the hard-float leandros-libc (DOOM). |
| [`limine/`](../../limine) | `limine.conf` copied into the boot image. |
| [`docs/`](../README.md) | This handbook and the historic [design records](../design/README.md). |
| [`TODO.md`](../../TODO.md) | The live tracker of open work. |

Build outputs land in the repository root and are gitignored: `leandros-limine-<arch>.img` (the boot image), `f2fs-data0-<arch>.img` (the populated root volume) and `f2fs-data1-<arch>.img` (the persistent `/home` volume, kept across builds), `initrd-<arch>.cpio[.gz]`, and `target/final-<arch>/` (kernel ELFs and direct-boot binaries).

## Sibling repositories

Large dependencies live outside this tree, as siblings of the repository root (`../<name>`), and are compiled by [`scripts/build-all.sh`](../../scripts/build-all.sh). A missing sibling is skipped with a warning, except where noted.

| Path | Purpose |
|---|---|
| `../brush` | The `brush` shell. Must be the pinned revision plus `ports/brush/patches` — `ports/brush/sync.sh apply ../brush` sets it up; the build refuses an off-pin checkout unless `LEANDROS_BRUSH_UNCHECKED=1`. Path overridable with `LEANDROS_BRUSH_DIR`. |
| `../crossterm` | The crossterm fork brush builds against through `[patch.crates-io]` |
| `../coreutils` | uutils coreutils |
| `../cosmic-epoch` | COSMIC desktop sources (built unmodified) and the source of `xdg-desktop-portal-cosmic` |
| `../doomgeneric` | DOOM port; also triggers the SoundFont download |
| `../mame` | MAME (or the directory in `$LEANDROS_MAME_DIR`) |
| `../bottom-leandros` | The `bottom` process monitor |
| `../disks-rs` | AerynOS's disk provisioning crates (`disktester`), built unmodified |

The image builder also reads prebuilt staging trees (the COSMIC session binaries, the Mesa GPU stack, D-Bus session payloads, wallpaper and icon data) from an out-of-tree directory, `~/code/leandros-artifacts`, whose path is hard-coded in [`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py). It is hand-synced per machine and is a known source of machine-to-machine differences; see [workflow](../development/workflow.md#build-outputs-and-image-provenance).

Development worktrees must themselves be siblings of the repository root (`../leandros-<lane>`), because the build resolves these paths relative to the tree ([workflow](../development/workflow.md)).

## See also

- [Prerequisites](../getting-started/prerequisites.md) — what to install before building.
- [Building](../getting-started/building.md) — what `build-all.sh` does with all of the above.
