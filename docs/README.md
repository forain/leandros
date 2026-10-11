# The LeandrOS Handbook

LeandrOS is a `no_std` microkernel written in Rust for **x86-64** and **AArch64**, with a Linux-compatible syscall personality that runs unmodified `*-unknown-linux-musl` software — up to a full COSMIC Wayland desktop. This handbook describes how each part of the kernel and the operating system works and, above all, **why it was designed that way**.

New here? Read [Overview](overview/README.md), then [Getting started](getting-started/README.md).

## Contents

### 1. [Overview](overview/README.md)
- [What LeandrOS is](overview/README.md) — current state and architecture at a glance
- [Design principles](overview/design-principles.md) — the cross-cutting rules every subsystem follows
- [Workspace layout](overview/workspace-layout.md) — crates, directories, and sibling repositories
- [History](overview/history.md) — milestone timeline

### 2. [Getting started](getting-started/README.md)
- [Prerequisites](getting-started/prerequisites.md) — toolchain, QEMU, linker, sibling repos, musl cross toolchain
- [Building](getting-started/building.md) — `build-all.sh`, release-only builds
- [Running in QEMU](getting-started/running.md) — boot modes, accelerators, disks, networking, GPU modes
- [Logging in](getting-started/logging-in.md) — graphical and serial login, accounts

### 3. [Kernel](kernel/README.md)
- [Boot](kernel/boot.md) — Limine and DTB boot paths, hand-off to init
- [Memory management](kernel/memory.md) — buddy, slab, VMM, demand paging, CoW, the unified page cache, W^X, user-copy discipline
- [Scheduler and SMP](kernel/scheduler.md) — run queue, context switch, CPU bring-up, watchdog
- [IPC](kernel/ipc.md) — ports, messages, `sys_call`, auxv service discovery
- [System calls](kernel/syscalls.md) — the native ABI and the Linux personality
- [Processes](kernel/processes.md) — thread groups, `fork`/`execve`, the ELF loader, credentials
- [Threads and futexes](kernel/threads-and-futex.md) — `clone(CLONE_VM)`, `futex`, the pthreads split
- [Signals](kernel/signals.md) — delivery, frames, `sigaltstack`, job-control stops
- [Time and timers](kernel/time.md) — the monotonic clock, POSIX timers, `timerfd`, sleeping
- [The event loop](kernel/event-loop.md) — `poll`/`select`/`epoll`, wait channels, readiness edges
- [x86-64](kernel/arch-x86_64.md) and [AArch64](kernel/arch-aarch64.md) architecture support
- [Kernel invariants](kernel/invariants.md) — the hard rules, and the bugs that taught them

### 4. [Servers](servers/README.md)
- [VFS](servers/vfs.md) — fd tables, mounts, tmpfs, shared memory, `/proc`, `/sys`
- [Filesystems and storage](servers/filesystems.md) — F2FS (including performance on slow media and the storage knob table), xattrs and POSIX ACLs, block devices
- [Networking](servers/networking.md) — smoltcp TCP/IP and a complete AF_UNIX
- [TTYs and virtual terminals](servers/tty-and-vt.md) — line discipline, job control, PTYs, VT switching
- [Users and security](servers/security.md) — accounts, login, permission enforcement

### 5. Devices and media
- [Graphics](graphics/README.md) — [DRM/KMS](graphics/drm-kms.md), [virtio-gpu](graphics/virtio-gpu.md), [console](graphics/console.md), [Vulkan via Venus](graphics/vulkan-venus.md), [Zink and virgl](graphics/zink-and-virgl.md), [Mesa](graphics/mesa.md)
- [Audio](audio.md) — virtio-snd, Intel HD Audio, the PipeWire server, `aplay`
- [Input](input.md) — evdev, virtio-input, libinput and the seat/udev shims

### 6. [Desktop](desktop/README.md)
- [Graphical login](desktop/login.md) — greetd, cosmic-greeter, `pam-leandros`
- [The COSMIC session](desktop/session.md) — cosmic-session, D-Bus, panel and applets
- [Limitations](desktop/limitations.md) — what the desktop still cannot do

### 7. [Userland](userland/README.md)
- [C libraries](userland/libc.md) — relibc, leandros-libc, musl
- [Programs](userland/programs.md) — init, login, shells, utilities
- [Ports](userland/ports.md) — third-party software and how it is built

### 8. [Hardware](hardware/README.md)
- [Virtual machines](hardware/virtual-machines.md) — QEMU, HVF/KVM/TCG, UTM
- [Raspberry Pi 5](hardware/raspberry-pi-5.md) — building, SD card preparation, bring-up
- [PC hardware](hardware/pc.md) — USB boot, USB input, NVIDIA, r8169, HD Audio, storage on a slow stick
- [Drivers](hardware/drivers.md) — PCI, virtio, SDHCI, USB, Wi-Fi

### 9. [Development](development/README.md)
- [Workflow](development/workflow.md) — machines, work lanes, the QEMU driver
- [Testing](development/testing.md) — regression suites and how to trust them
- [Debugging](development/debugging.md) — watchdog lines, task dumps, capture pitfalls
- [Methodology](development/methodology.md) — measuring at the destination, refuted premises
- [Conventions](development/conventions.md) — project rules for code and commits

### Appendix
- [Design records](design/README.md) — the original design documents, kept verbatim
- [`TODO.md`](../TODO.md) — the live work tracker
- License: GPL-3.0 ([`LICENSE`](../LICENSE))
