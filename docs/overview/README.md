# What LeandrOS is

LeandrOS is a `no_std`, bare-metal microkernel written in Rust for **x86-64** (UEFI, QEMU q35, real PCs) and **AArch64** (QEMU `virt`, Raspberry Pi 5). On top of a small microkernel core it implements the **Linux syscall ABI**, so unmodified `*-unknown-linux-musl` binaries — a shell, coreutils, Mesa, a Wayland compositor, the COSMIC desktop, Firefox — run without a source patch or a shim ABI.

## Overview

The kernel core provides scheduling, IPC and memory management. Everything else — the VFS, file systems, the DRM/KMS graphics stack, audio, input, terminals, networking — is structured as **servers**: separate crates that own their state and are reached through typed IPC ports. A **Linux-compatible syscall personality** sits in front of all of it: the dispatcher implements the real Linux syscall numbers for each architecture and routes each call to the core or to the server that owns the resource.

The goal is that a Linux program cannot tell the difference. When LeandrOS and Linux disagree, LeandrOS is wrong, and the fix belongs in the kernel, not in the program (see [design principles](design-principles.md)).

## Current state (November 2026)

- **Graphical login by default, on both architectures.** PID 1 (`userland/init`) supervises `greetd` + `cosmic-greeter` on the display, a serial `login:` getty on the serial console, and text logins on virtual terminals 2–6. The greeter runs unprivileged (uid 990), authenticates against `/etc/shadow` (`root`/`root` or `leandro`/`leandro`), and hands off to a full **COSMIC desktop**: `cosmic-session` → `cosmic-comp` → `busd` (D-Bus) → `cosmic-bg`, `cosmic-panel`, the panel and dock applets, `cosmic-term` on a kernel PTY, COSMIC Settings, D-Bus service activation and an xdg-desktop-portal; Ctrl+Alt+F1–F6 switches virtual terminals, with display and input arbitrated per VT. See [Logging in](../getting-started/logging-in.md) and [the desktop stack](../desktop/README.md).
- **COSMIC is unmodified** apart from three narrowly scoped upstream-bug patches to `cosmic-comp` (a closed-window texture leak, frame callbacks while an output is powered off, and an idle inhibitor waking a powered-off screen; `ports/cosmic-comp/`). None dodges a cost of our own; every other incompatibility was fixed on the kernel side.
- **COSMIC renders on the host GPU only.** The runner selects a GPU path by default: Venus (Vulkan) with Zink on top where the host supports it, virgl (OpenGL passthrough) where only GL exists. A guest without a hardware renderer refuses to start the graphical login rather than fall back to software rendering; software rendering (softpipe) is an explicit opt-in for debugging. See [Running](../getting-started/running.md) and [graphics](../graphics/README.md).
- **Ported software** includes the `brush` shell (with job control), uutils coreutils, Mesa, PipeWire/WirePlumber, Firefox (with http/https/HTTP3 networking), DOOM (as a Wayland client with PipeWire sound), MAME, AerynOS's `disks-rs` provisioning tool (loop devices, GPT, mkfs.fat/mkfs.xfs), `podman` (root and rootless `run`, with bridge and `pasta` networking), Flatpak (the real `bubblewrap` and a `libflatpak` port that `cosmic-store` uses, plus a PackageKit facade over `apk`) and a real Linux-PAM behind `sudo`, `su` and `greetd`. Firefox, PipeWire, Mesa and the container tools are staged from Alpine packages; the image tracks Alpine's rolling `edge` release, with `ports/alpine/*.lock` pinning exactly what was resolved. See [ports](../userland/ports.md).
- **Real hardware**: a real x86-64 PC boots from a USB stick with the root file system on it; NVIDIA's open GPU kernel modules drive an RTX 3070 (COSMIC, Firefox with YouTube and sound). An HP EliteBook (all-Intel) boots COSMIC on i915 with Wi-Fi, a touchpad, a webcam, external displays, battery and backlight, USB Bluetooth and microphone capture. The Raspberry Pi 5 has Wi-Fi, Ethernet (RP1), USB, Bluetooth, HDMI audio, SD storage (UHS-I and ADMA2, and a driver that never writes a card that is not LeandrOS's own) and a V3D GPU driver that Mesa renders on, with the COSMIC desktop scanned out through the HVS. See [hardware](../hardware/README.md).
- **kexec**: a running LeandrOS can boot another kernel image, on both architectures and on the real laptop and Pi, which is also how kernels are updated over the network.
- **Loadable kernel modules**: drivers are configured Y/M/N with `scripts/menuconfig.sh`; many (network, block, sound, USB, Wi-Fi, Bluetooth) build as modules loaded by alias. See [kernel modules](../kernel/modules.md).
- **Toolchain**: all host-side C and C++ (Mesa, COSMIC, MAME, the in-tree tests) builds with clang and lld against a locked Alpine musl sysroot, the same musl the image ships; zig is no longer used or needed ([prerequisites](../getting-started/prerequisites.md#the-clanglld-musl-cross-toolchain)).
- **Test farm**: a Linux desktop runs the full regression on both architectures in parallel, detached from the submitting machine ([workflow](../development/workflow.md#locks-the-regress-farm-and-lane-discipline)); `runtests.py --shards K` runs one regress as K concurrent boots, and long suites live in a separate `slow` group ([testing](../development/testing.md#running-the-suites)). The build also works on a Linux host, not only macOS ([prerequisites](../getting-started/prerequisites.md)).
- **Device updates**: `scripts/device-update.sh` deploys a freshly built kernel to the laptop or the Pi, boots it once, verifies it and promotes it only if every check passed ([remote](../development/remote.md)).
- **Bluetooth**: `btd` and `bthid` in userland over a raw HCI node, with an `org.bluez` shim for the COSMIC Bluetooth applet.

The dated record of how it got here is in [History](history.md); the live list of open work is [TODO.md](../../TODO.md).

## Architecture at a glance

```
┌──────────────────────────────────────────────────────────────┐
│                  User space (EL0 / Ring 3)                   │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐      │
│  │  COSMIC  │  │  brush   │  │ Firefox  │  │  aplay,  │      │
│  │ desktop  │  │  shell   │  │  DOOM …  │  │ tests …  │      │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘      │
│  ┌────┴─────────────┴─────────────┴─────────────┴─────┐      │
│  │ Mesa (Zink/Venus, virgl) · libinput · musl  │      │
│  └────────────────────────┬───────────────────────────┘      │
├───────────────────────────┼──────────────────────────────────┤
│                     Linux syscall ABI                        │
├───────────────────────────┼──────────────────────────────────┤
│                 Kernel (EL1 / Ring 0)                        │
│  ┌────────────────────────┴───────────────────────────┐      │
│  │ syscall personality: Linux numbers, path resolver, │      │
│  │ fd tables, signals, futexes, poll/epoll            │      │
│  └───────┬──────────────────────────────────┬─────────┘      │
│          │ IPC ports (send / recv / call)   │                │
│  ┌───────┴──────────────────────────────────┴─────────┐      │
│  │ Servers: VFS · f2fs · xattr/ACL · DRM · PipeWire · │      │
│  │          evdev · tty/VT/PTY · net (smoltcp, unix)  │      │
│  └───────┬──────────────────────────────────┬─────────┘      │
│  ┌───────┴───────┐ ┌──────────────┐ ┌───────┴────────┐       │
│  │ scheduler,    │ │ IPC ports,   │ │ MM: buddy,     │       │
│  │ SMP, ELF exec │ │ messages     │ │ slab, VMM, CoW │       │
│  └───────────────┘ └──────────────┘ └────────────────┘       │
│  ┌───────────────┐ ┌──────────────┐ ┌────────────────┐       │
│  │ boot: Limine, │ │ drivers:     │ │ arch: x86_64 / │       │
│  │ DTB, direct   │ │ virtio, PCI, │ │ aarch64 traps, │       │
│  │               │ │ USB, SDHCI…  │ │ IRQs, timers   │       │
│  └───────────────┘ └──────────────┘ └────────────────┘       │
└──────────────────────────────────────────────────────────────┘
```

**IPC model.** Components communicate through *ports* (bounded message queues). The kernel exposes three primitives: `send` (non-blocking enqueue), `recv` (blocking dequeue on an owned port) and `call` (send, then block on a private reply port). There is no shared memory between tasks unless it is explicitly mapped. Server port ids are stamped into each process's ELF auxiliary vector, so a client finds its servers in O(1) without a name service. Details: [IPC](../kernel/ipc.md).

**Where the servers run.** The server crates (`servers/*`) are linked into the kernel image and registered at boot by [`kernel/src/init.rs`](../../kernel/src/init.rs) ("in-kernel servers"); a port can carry a *direct handler* that the IPC layer calls synchronously in the caller's context ([`ipc/src/port.rs`](../../ipc/src/port.rs)). The microkernel split is therefore today a **structural** boundary — separate crates, private state, a message interface — rather than a privilege boundary. The consequence shows up throughout the handbook: server code runs at kernel privilege and must follow the kernel's rules, for example never touching user memory while holding a scheduler lock ([kernel invariants](../kernel/invariants.md)). See [the server layer](../servers/README.md).

## In this section

- [Design principles](design-principles.md) — the cross-cutting rules every subsystem follows, and why.
- [Workspace layout](workspace-layout.md) — crates, directories and the sibling repositories the build uses.
- [History](history.md) — a dated milestone timeline with commit ids.

## See also

- [Getting started](../getting-started/README.md) — build and boot it.
- [Kernel](../kernel/README.md), [servers](../servers/README.md), [graphics](../graphics/README.md), [desktop](../desktop/README.md).
- [Development](../development/README.md) — workflow, testing, debugging.
