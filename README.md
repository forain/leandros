# Leandros

A `no_std` bare-metal microkernel written in Rust, targeting **x86-64** (UEFI/QEMU) and **AArch64** (QEMU virt, Raspberry Pi 5).

Leandros follows the classic microkernel design: the kernel itself provides only scheduling, IPC, and memory management. Everything else — drivers, file systems, network stacks — runs as isolated user-space tasks that communicate via typed message passing.

---

## Architecture at a glance

```
┌───────────────────────────────────────────────────────────┐
│                  User Space (EL0 / Ring 3)                 │
│  ┌──────────┐  ┌──────────┐  ┌─────────────┐  ┌─────────┐ │
│  │   init   │  │  shell   │  │   driver    │  │ server  │ │
│  │ (PID-1)  │  │   CLI    │  │   tasks     │  │  tasks  │ │
│  │ ELF load │  │ commands │  │             │  │   VFS   │ │
│  └────┬─────┘  └────┬─────┘  └──────┬──────┘  └────┬────┘ │
│       │  SVC / SYSCALL  │            │              │      │
└───────┼─────────────────┼────────────┼──────────────┼──────┘
        ↓    Kernel Space (EL1 / Ring 0)               ↓
┌───────────────────────────────────────────────────────────┐
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           │
│ │   syscall   │ │ IPC ports   │ │ scheduler   │           │
│ │  dispatch   │ │ messaging   │ │ ELF loader  │           │
│ └─────────────┘ └─────────────┘ └─────────────┘           │
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           │
│ │     MM      │ │   paging    │ │    arch     │           │
│ │ buddy+slab  │ │ VMM+demand  │ │ debug+init  │           │
│ │   1MB stack │ │   W^X       │ │ FP/SIMD en. │           │
│ └─────────────┘ └─────────────┘ └─────────────┘           │
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           │
│ │ boot parse  │ │   drivers   │ │ kernel shell│           │
│ │ DTB+Limine  │ │ serial+FB   │ │ help/info   │           │
│ │ QEMU fallbk │ │   USB+WiFi  │ │ interactive │           │
│ └─────────────┘ └─────────────┘ └─────────────┘           │
└───────────────────────────────────────────────────────────┘
```

**IPC model** — processes communicate exclusively through *ports* (bounded message queues). The kernel exposes three primitives: `send` (non-blocking enqueue), `recv` (blocking dequeue on owned port), and `call` (send + block on private reply port). There is no shared memory between tasks unless explicitly mapped.

---

## Workspace layout

| Crate | Purpose |
|---|---|
| `kernel` | Entry point, `kernel_main`, syscall dispatch, init task, kernel shell |
| `mm` | Buddy allocator, slab allocator, VMM, page-table interface, ELF mapping |
| `sched` | Cooperative/preemptive scheduler, context switch, IPC blocking, ELF loading |
| `ipc` | Port table, message types, `Channel` abstraction |
| `boot` | Multiboot2, Limine, and Device Tree (FDT) parsers → `BootInfo` |
| `arch/x86_64` | GDT/TSS, IDT, APIC, PIC, SYSCALL entry, SMP, timer |
| `arch/aarch64` | MMU, exception vectors, GICv2, generic timer, UART, SMP/PSCI, debug utils |
| `drivers` | PL011/16550 serial, linear framebuffer |
| `drivers/usb` | xHCI host controller |
| `drivers/wifi` | mac80211 + virtio-wifi |
| `userland` | User-space programs (init, shell, hello) with leandros-libc |
| `lib` | `align_up` / `align_down` utilities shared across crates |

---

## Supported targets

| Target | Boot protocol | Status |
|---|---|---|
| x86-64 (QEMU q35) | Limine UEFI | Working |
| AArch64 (QEMU virt) | Device Tree (DTB via `-kernel`) | Working |
| Raspberry Pi 5 | RPi firmware ELF load + BCM2712 DTB | Boot-ready |

---

## Prerequisites

### Toolchain

Leandros requires a Rust **nightly** toolchain with bare-metal cross-compilation targets. The `rust-toolchain.toml` at the repo root pins the exact channel and fetches all required components automatically on first build.

```
rustup show   # confirms toolchain is active
```

### QEMU (x86-64)

```sh
# Debian / Ubuntu
sudo apt install qemu-system-x86 ovmf dosfstools mtools

# Arch Linux
sudo pacman -S qemu-system-x86 edk2-ovmf dosfstools mtools

# Fedora
sudo dnf install qemu-system-x86 edk2-ovmf dosfstools mtools
```

### QEMU (AArch64)

```sh
sudo apt install qemu-system-arm     # Debian/Ubuntu
sudo pacman -S qemu-system-aarch64   # Arch
```

### Linker

```sh
sudo apt install lld    # ld.lld is used for both targets
```

---

## Building

The workspace default target is `aarch64-unknown-none`. Pass `--target` to switch.

⚠️ **Important**: Use release builds for testing — debug builds may hang during early boot due to large stack requirements.

```sh
# AArch64 — QEMU virt (recommended: release mode)
cargo build --release --target aarch64-unknown-none

# AArch64 — Raspberry Pi 5
cargo build --release --target aarch64-unknown-none --features rpi5

# x86-64 — Limine UEFI
cargo build --release --target x86_64-unknown-none

# Debug builds (use only for development with additional tooling)
cargo build --target aarch64-unknown-none
```

---

## Running in QEMU

### AArch64

```sh
# Release mode (recommended for actual testing)
cargo run --release --target aarch64-unknown-none

# Debug mode (for development only)
cargo run --target aarch64-unknown-none
```

QEMU is configured as the default runner for `aarch64-unknown-none` in `.cargo/config.toml`. It boots the ELF directly with `-kernel`, passing the virt machine's built-in DTB in `x0`.

**Note**: The kernel now includes a comprehensive init system with:
- ELF loading capabilities for userspace programs
- Interactive kernel shell with commands (`help`, `info`, `test`)
- Enhanced debugging and exception handling
- Improved memory management with 1MB kernel stack

### x86-64

```sh
cargo run --target x86_64-unknown-none
```

The runner script (`scripts/run-x86_64.sh`) builds a fresh FAT32 disk image containing Limine v11 and the kernel ELF, then launches QEMU with OVMF. Limine is fetched from GitHub on the first run and cached in `target/limine/`.

---

## Deploying to Raspberry Pi 5

Build with the `rpi5` feature to select the correct UART, GIC, and MMU addresses for the BCM2712 SoC, and to link at the RPi firmware's expected load address (`0x80000`).

```sh
cargo build --release \
    --target aarch64-unknown-none \
    --features rpi5 \
    -p kernel
```

Copy to an SD card that already has RPi 5 firmware (`start4.elf`, `fixup4.dat`, `bcm2712-rpi-5-b.dtb`) on its FAT32 boot partition:

```sh
sudo ./scripts/deploy-rpi5.sh \
    target/aarch64-unknown-none/release/kernel \
    /dev/mmcblk0
```

The script writes `kernel.elf` and updates `config.txt` (`kernel=kernel.elf`, `arm_64bit=1`). The RPi firmware reads the load address from the ELF's `PT_LOAD` header.

**First-time SD card setup** (one-off): flash Raspberry Pi OS Lite, or manually copy the [RPi firmware files](https://github.com/raspberrypi/firmware/tree/master/boot) to a FAT32 partition.

---

## Kernel subsystems

### Memory management (`mm`)

- **Buddy allocator** — power-of-two physical page allocator (up to 4 MiB contiguous blocks, order 0–10). Initialised from the boot memory map; firmware-reserved regions (from the FDT `/memreserve/` block) are excluded automatically.
- **Slab allocator** — fixed-size object caches (8 B – 4 KiB, powers of two) backed by the buddy allocator. Requests larger than one page fall through to the buddy allocator directly.
- **VMM** — per-process `AddressSpace` holding a list of `VmaRegion` descriptors. Supports eager (`map`) and demand-paged (`map_lazy`) mappings. Lazy VMAs fault in individual 4 KiB pages on access; W^X is enforced at the syscall boundary.

### Scheduler (`sched`)

- Cooperative + preemptive round-robin, with per-task signed priority.
- Context switch saves/restores all callee-saved integer registers **and** FPU/SIMD state (Q0–Q31 on AArch64; XMM0–XMM15 + MXCSR on x86-64) on every switch.
- **ELF loading** — direct userspace program loading with proper memory mapping and entry point setup.
- Tasks block on IPC ports (`block_on(port)`) and are unblocked by `send` or port close.
- SMP: up to 8 CPUs. BSP runs `sched::run()`; APs are started via PSCI `CPU_ON` (AArch64) and SIPI (x86-64), then enter `sched::ap_entry()`.
- `wait_pid` uses an exit-log side-table to avoid the race where the scheduler reaps a zombie before the waiter resumes.
- **Enhanced debugging** — detailed task state monitoring and exception diagnostics.

### IPC (`ipc`)

- **Ports** — bounded FIFO queues (16 messages each). Created with `port::create(owner_pid)`; only the owner may `recv`. Any task may `send` to any port it holds the ID of.
- **Messages** — 64-byte inline payload (`MESSAGE_INLINE_BYTES = 48`), a `tag` word, a `reply_port` field (for `sys_call`), and one capability slot (`Option<usize>`).
- **`sys_call`** — send-and-wait idiom. The kernel lazily allocates a private *reply port* per task (cached in `Task::reply_port`), stamps it into the outgoing message, and blocks the caller on that port. Servers reply by sending to `msg.reply_port`.
- **`Channel`** — convenience wrapper pairing a client port and a server port; used by drivers that need a bidirectional rendezvous.

### Syscall ABI

| Number | Name | Args | Returns |
|---|---|---|---|
| 0 | `send` | port, msg_ptr | 0 / errno |
| 1 | `recv` | port, msg_ptr | 0 / errno |
| 2 | `call` | port, msg_ptr | 0 / errno |
| 3 | `map_mem` | virt, size, flags | 0 / errno |
| 4 | `unmap_mem` | virt, size | 0 |
| 5 | `yield` | — | 0 |
| 6 | `exit` | code | — |
| 7 | `spawn` | entry_va, stack_va, priority | pid / errno |
| 8 | `clock_gettime` | dest_ptr | 0 / errno |
| 9 | `wait` | pid, status_ptr | 0 / errno |

Register mapping follows the Linux convention on each architecture:

- **AArch64**: syscall number in `x8`, args in `x0`–`x2`, return value in `x0`. Entry via `svc #0`.
- **x86-64**: syscall number in `rax`, args in `rdi`/`rsi`/`rdx`, return value in `rax`. Entry via `syscall` instruction (STAR/LSTAR MSRs).

### Boot flow

**x86-64 (Limine)**

```
OVMF → Limine UEFI app → fills static request structs in kernel image
     → jumps to _start (already in 64-bit long mode, paging on)
     → kernel_main(0)
     → boot::limine::parse()   — reads Limine response pointers
     → arch_x86_64::init()     — GDT/TSS, IDT, APIC, SYSCALL
     → mm::init_with_map()
     → sched::init() + ipc::init()
     → spawn init task → sched::run()
```

**AArch64 (QEMU virt / RPi 5)**

```
Firmware → _start (MMU off, x0 = DTB physical address)
         → park secondary CPUs
         → EL2 → EL1 drop if needed (RPi 5 boots at EL2)
         → zero BSS, install VBAR_EL1
         → kernel_main(dtb_ptr)
         → arch_aarch64::init()  — MAIR, MMU identity map, GICv2, timer
         → boot::device_tree::parse(dtb_ptr)
         → mm::init_with_map()   — honours /memreserve/ entries
         → sched::init() + ipc::init()
         → spawn init task → sched::run()
```

---

## Key design decisions

**No `unsafe` globals beyond boot parsers** — static mutable state is wrapped in `spin::Mutex` throughout the kernel. The only bare `static mut` blocks are in the boot parsers (run single-threaded before any AP starts) and the arch assembly stubs.

**W^X enforced** — `sys_map_mem` rejects any mapping with both `WRITABLE` and `EXECUTE` flags set.

**Checked arithmetic at memory boundaries** — VMA end-address calculations, slab order arithmetic, and all DTB offset reads use `checked_add` rather than wrapping arithmetic. The slab allocator returns `None` (OOM) rather than silently under-allocating for requests above the maximum buddy order.

**Demand paging** — user tasks call `sys_map_mem` with a lazy flag to reserve virtual address space without touching physical memory. Each page is allocated and mapped on first access via the page-fault handler.

**Per-CPU SYSCALL stacks (x86-64)** — each CPU has a `PerCpuSyscall { kernel_stack_top, user_rsp_save }` struct pointed at by `IA32_KERNEL_GS_BASE`. The syscall entry stub uses `swapgs` + `%gs:0/8` to load the kernel stack and save the user RSP without touching any shared state.

**Exit-log side-table** — `sched::run()` reaps zombie tasks immediately after they exit, but records their exit code in a 256-slot `EXIT_LOG` table keyed by PID. `wait_pid` falls back to this table when `find_pid` returns `None`, eliminating the race between the reaper and the waiter.

**Enhanced debugging** — comprehensive exception analysis on AArch64 with detailed data abort information, memory attribute debugging, and task state monitoring for development and troubleshooting.

**Optimized builds** — release builds use LTO, symbol stripping, and 4KB page alignment for reduced binary size and improved performance. Debug builds require larger stacks due to unoptimized code paths.

---

## Userland programs

LeandrOS includes a userland development framework with a minimal C runtime (`leandros-libc`):

### Built-in programs

- **`init`** — PID-1 initialization process with ELF loading capabilities
- **`shell`** — Interactive command-line interface with basic Unix commands
- **`hello`** — Simple "Hello, world!" demonstration program

### Building userland

```sh
# Build all userland programs in release mode
./scripts/build-userland.sh --release

# Build in debug mode (larger binaries)
./scripts/build-userland.sh
```

The userland build system creates statically-linked binaries using a custom `leandros-libc` that provides:
- Linux-compatible syscall ABI (AArch64)
- Basic libc functions (`printf`, `malloc`, `memcpy`, etc.)
- Process management (`fork`, `exec`, `wait`)
- File I/O (when VFS is available)

Programs are embedded directly in the kernel image for simplified distribution and early boot access.

---

## License

GPL-3.0
