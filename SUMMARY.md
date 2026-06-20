# LeandrOS - Comprehensive Summary

LeandrOS is a modern, bare-metal microkernel written in Rust that targets both x86-64 and AArch64 architectures. It implements a classic microkernel design where the kernel provides only core services (scheduling, IPC, memory management) while all other functionality runs as isolated user-space tasks.

## Project Structure

The codebase is organized into a Cargo workspace with the following main components:

1. **Kernel Layer** (`kernel/`) - The core microkernel with entry points and system management
2. **Memory Management** (`mm/`) - Buddy allocator, slab allocator, VMM, page-table interface
3. **Scheduler** (`sched/`) - Cooperative/preemptive scheduler with ELF loading and IPC blocking
4. **IPC System** (`ipc/`) - Port-based messaging system with bounded queues
5. **Boot Module** (`boot/`) - Parses boot information from Limine, Multiboot2, and Device Tree
6. **Architecture-Specific Code** (`arch/x86_64/`, `arch/aarch64/`) - CPU-specific implementations
7. **Drivers** (`drivers/`) - Hardware drivers including KMS, DRM, VirtIO GPU/Sound, PCI, etc.
8. **Servers** (`servers/`) - User-space server tasks that provide services like VFS, DRM, evdev, PipeWire, etc.
9. **Userland Programs** (`userland/`) - Applications including init, shell, aplay, and hello
10. **C Library** (`userland/libc/`) - C runtime interface for userspace programs

## Key Features

### Microkernel Architecture
- **Minimal kernel**: Only provides scheduling, IPC, and memory management
- **User-space servers**: All drivers and services run as isolated user-space tasks
- **Typed message passing**: Processes communicate exclusively through ports (bounded message queues)
- **No shared memory**: Unless explicitly mapped, tasks cannot access each other's memory

### System Call Interface
- Implements Linux syscall ABI for compatibility with existing software
- Supports standard syscalls like `mmap`, `open`, `read`, `write`, `execve`, etc.
- Leandros-specific syscalls for IPC (`send`, `recv`, `call`) and process management

### Memory Management
- **Buddy allocator**: Power-of-two physical page allocator (up to 4 MiB contiguous blocks)
- **Slab allocator**: Fixed-size object caches (8 B – 4 KiB) backed by buddy allocator
- **VMM**: Per-process address space with eager and demand-paged mappings
- **W^X enforcement**: Prevents pages from being both writable and executable
- **SMAP support**: Supervisor-mode access prevention for safe kernel-to-userspace access

### Scheduler
- Cooperative + preemptive round-robin scheduling with per-task signed priority
- Context switching saves/restores all callee-saved registers and FPU/SIMD state
- ELF loading with proper memory mapping and entry point setup
- SMP support for up to 8 CPUs with PSCI and SIPI support
- Auxv-based service discovery for efficient server port access

### IPC System
- **Ports**: Bounded FIFO queues (16 messages each) with ownership semantics
- **Messages**: 64-byte inline payload with tag, reply port, and capability slot
- **Operations**: `send` (non-blocking), `recv` (blocking), `call` (send + wait)
- **Channels**: Convenience wrapper for bidirectional rendezvous

### Hardware Support
- **Graphics**: KMS with EDID autodetection, DRM subsystem, VirtIO GPU driver
- **Audio**: VirtIO Sound driver with PipeWire server for audio processing
- **Input**: evdev server for keyboard and mouse input
- **Network**: Network server with TUN/TAP support
- **Storage**: VirtIO block device support with F2FS filesystem

### Boot Process
- **x86-64**: Uses Limine UEFI bootloader with OVMF
- **AArch64**: Uses Device Tree (DTB) parsing for QEMU virt and Raspberry Pi 5
- **Boot protocols**: Supports Limine revision 6, Multiboot2, and Device Tree
- **Memory map**: Parses boot memory map and excludes firmware-reserved regions

### Userland Components
- **Init**: PID-1 process that spawns servers and user tasks
- **Shell**: Interactive CLI with Unix-style commands and VFS integration
- **Audio Player**: `aplay` for playing WAV/MIDI files and test tones
- **Hello World**: Simple demonstration program
- **C Runtime**: Uses relibc (Redox OS C library) for full POSIX compatibility

## Build and Testing

The project uses a comprehensive build system with:
- **Cross-compilation**: Targets both x86-64 and AArch64 architectures
- **Release builds only**: Debug builds are avoided due to early boot issues
- **QEMU testing**: Scripts for testing both architectures in QEMU
- **Raspberry Pi 5 support**: Boot-ready with proper UART, GIC, and MMU settings

## Design Philosophy

1. **Safety**: No `unsafe` globals beyond boot parsers; static mutable state wrapped in `spin::Mutex`
2. **Security**: W^X enforcement, SMAP, checked arithmetic at memory boundaries
3. **Performance**: Demand paging, per-CPU syscall stacks, asynchronous audio pipeline
4. **Compatibility**: Linux syscall ABI compatibility, POSIX C library support
5. **Modularity**: Clear separation between kernel and user-space components

## Key Technical Details

- **Rust Nightly**: Requires Rust nightly toolchain with bare-metal targets
- **Limine Bootloader**: Uses Limine revision 6 for x86-64 boot
- **Device Tree**: AArch64 uses Device Tree for hardware description
- **Memory Layout**: Uses HHDM (Higher Half Direct Map) for kernel memory management
- **System Call Numbers**: Follow Linux ABI for compatibility with musl libc