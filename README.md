# Leandros

A `no_std` bare-metal microkernel written in Rust, targeting **x86-64** (UEFI/QEMU) and **AArch64** (QEMU virt, Raspberry Pi 5).

Leandros follows the classic microkernel design: the kernel itself provides only scheduling, IPC, and memory management. Everything else — drivers, file systems, network stacks — runs as isolated user-space tasks that communicate via typed message passing.

---

## Architecture at a glance

```
┌──────────────────────────────────────────────────────────┐
│                     User Space (EL0 / Ring 3)            │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
│  │   init   │  │  shell   │  │  aplay   │  │  other   │  │
│  │ (PID-1)  │  │   CLI    │  │ WAV/MIDI │  │  tasks   │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  │
│       │   SYSCALL   │             │             │        │
├───────┼─────────────┼─────────────┼─────────────┼────────┤
│                 Server Layer (EL0 / Ring 3)              │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
│  │   VFS    │  │   DRM    │  │ PipeWire │  │  evdev   │  │
│  │  server  │  │  server  │  │  server  │  │  server  │  │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘  │
└──────────────────────────────────────────────────────────┘
        ↓        Kernel Space (EL1 / Ring 0)
┌──────────────────────────────────────────────────────────┐
│      ┌─────────────┐ ┌─────────────┐ ┌─────────────┐     │
│      │   syscall   │ │ IPC ports   │ │ scheduler   │     │
│      │  dispatch   │ │ messaging   │ │ ELF loader  │     │
│      └─────────────┘ └─────────────┘ └─────────────┘     │
│      ┌─────────────┐ ┌─────────────┐ ┌─────────────┐     │
│      │     MM      │ │   paging    │ │    arch     │     │
│      │ buddy+slab  │ │ VMM+demand  │ │ debug+init  │     │
│      │   1MB stack │ │   W^X/SMAP  │ │ FP/SIMD en. │     │
│      └─────────────┘ └─────────────┘ └─────────────┘     │
│      ┌─────────────┐ ┌─────────────┐ ┌─────────────┐     │
│      │ boot parse  │ │   drivers   │ │ kernel shell│     │
│      │ DTB+Limine  │ │ KMS/DRM/GPU │ │ help/info   │     │
│      │ QEMU fallbk │ │ VirtIO SND  │ │ interactive │     │
│      └─────────────┘ └─────────────┘ └─────────────┘     │
└──────────────────────────────────────────────────────────┘
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
| `drivers` | KMS, DRM subsystem, VirtIO GPU, VirtIO Sound, framebuffer, serial, PCI |
| `drivers/usb` | xHCI host controller |
| `drivers/wifi` | mac80211 + virtio-wifi |
| `servers/vfs` | Virtual filesystem server |
| `servers/drm` | DRM server (hardware-accelerated graphics IPC) |
| `servers/evdev` | Linux-compatible input event server |
| `servers/pipewire` | PipeWire-compatible audio server |
| `servers/tty` | TTY server |
| `servers/net` | Network server |
| `userland` | User-space programs (init, shell, aplay) with leandros-libc / relibc |
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

Use the top-level build script to compile all targets:

```sh
./scripts/build-all.sh
```

⚠️ **Important**: Always use release builds — debug builds may hang during early boot due to large stack requirements and symbol desync issues.

---

## Running in QEMU

```sh
# Test both architectures
./scripts/run-qemu.sh aarch64
./scripts/run-qemu.sh x86_64
```

The AArch64 runner boots the ELF directly with `-kernel`, passing the virt machine's built-in DTB in `x0`. The x86-64 runner builds a fresh FAT32 disk image containing Limine (rev ≥ 6) and the kernel ELF, then launches QEMU with OVMF.

---

## Deploying to Raspberry Pi 5

Build with the `rpi5` feature to select the correct UART, GIC, and MMU addresses for the BCM2712 SoC, and to link at the RPi firmware's expected load address (`0x80000`):

```sh
./scripts/build-all.sh --arch aarch64 --rpi5
```

**First-time SD card setup**: `scripts/prepare-rpi5-sdcard.sh` wipes a blank SD card, creates the single MBR/FAT32 boot partition the Pi 5's boot ROM expects, and populates it for one of two boot paths (Linux and macOS both supported):

```sh
# RPi firmware loads kernel.elf directly, no bootloader in between
sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode direct /dev/mmcblk0   # or /dev/diskN on macOS

# RPi firmware loads the vendored RPI_EFI.fd, which boots Limine, which loads kernel.elf + initrd
sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode limine /dev/mmcblk0
```

It downloads and caches the Broadcom GPU firmware blobs (`start4.elf`, `fixup4.dat`) from the [RPi firmware repo](https://github.com/raspberrypi/firmware/tree/master/boot) on first run; pass `--firmware-dir <dir>` to supply them from a local copy instead.

After that first-time setup, `deploy-rpi5.sh` updates just the kernel ELF on an already-prepared card:

```sh
sudo ./scripts/deploy-rpi5.sh \
    target/final-aarch64/kernel-direct \
    /dev/mmcblk0
```

---

## Kernel subsystems

### Memory management (`mm`)

- **Buddy allocator** — power-of-two physical page allocator (up to 4 MiB contiguous blocks, order 0–10). Initialised from the boot memory map; firmware-reserved regions (from the FDT `/memreserve/` block) are excluded automatically.
- **Slab allocator** — fixed-size object caches (8 B – 4 KiB, powers of two) backed by the buddy allocator. Requests larger than one page fall through to the buddy allocator directly.
- **VMM** — per-process `AddressSpace` holding a list of `VmaRegion` descriptors. Supports eager (`map`) and demand-paged (`map_lazy`) mappings. Lazy VMAs fault in individual 4 KiB pages on access; W^X is enforced at the syscall boundary.
- **Kernel device mapping** — `map_kernel_device` provides identity mappings for MMIO regions (framebuffer, VirtIO BARs, etc.) with device-memory attributes, and exposes the page-table root for DRM mmap.
- **SMAP** — safe kernel-to-userspace memory access implemented via the architecture's supervisor-mode access-prevention facility.

### Scheduler (`sched`)

- Cooperative + preemptive round-robin, with per-task signed priority.
- Context switch saves/restores all callee-saved integer registers **and** FPU/SIMD state (Q0–Q31 on AArch64; XMM0–XMM15 + MXCSR on x86-64) on every switch.
- **ELF loading** — direct userspace program loading with proper memory mapping and entry point setup.
- Tasks block on IPC ports (`block_on(port)`) and are unblocked by `send` or port close.
- SMP: up to 8 CPUs. BSP runs `sched::run()`; APs are started via PSCI `CPU_ON` (AArch64) and SIPI (x86-64), then enter `sched::ap_entry()`.
- `wait_pid` uses an exit-log side-table to avoid the race where the scheduler reaps a zombie before the waiter resumes.
- **Auxv-based service discovery** — the kernel stamps server port numbers into the auxiliary vector at task creation. Userspace reads port IDs for audio, DRM, VFS, and other services from `AT_*` entries without a name-service round-trip.

### IPC (`ipc`)

- **Ports** — bounded FIFO queues (16 messages each). Created with `port::create(owner_pid)`; only the owner may `recv`. Any task may `send` to any port it holds the ID of.
- **Messages** — 64-byte inline payload (`MESSAGE_INLINE_BYTES = 48`), a `tag` word, a `reply_port` field (for `sys_call`), and one capability slot (`Option<usize>`). `reply_port` defaults to `u32::MAX` to prevent accidental recursive loops.
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

The table above is the original minimal IPC/memory surface; the dispatcher (`kernel/src/syscall.rs`) has since grown to cover the real Linux syscall numbers relibc expects — `clone`, `futex`, `rt_sigaction`, the `mmap` family, VFS, and networking calls all reuse their actual Linux numbers rather than a custom scheme, so relibc's `platform::linux` Pal implementation works unmodified against this kernel.

### Signal handling

Full POSIX-style signal delivery is implemented on **both** architectures:

- **Delivery path** — `check_and_deliver_signals()` runs on every return to userspace from a syscall, IRQ, or fault, on both AArch64 (`exception_asm.s`, EL0 paths only — the EL1 IRQ path deliberately skips it, since that path returns to interrupted kernel code) and x86-64 (`syscall_entry` return path in `arch/x86_64/src/syscall.rs`).
- **Signal frames** — real `rt_sigframe`s on both architectures; the x86-64 frame matches the SysV `ucontext`/`mcontext` layout expected by relibc's Linux-ABI `__restore_rt` trampoline.
- **`sigaltstack`** — real per-thread alt-stack state (`Task::altstack_sp/size/flags`). `SA_ONSTACK` redirects signal delivery onto the configured alt-stack; `SS_ONSTACK`/`EPERM`-while-active are derived from the live user stack pointer at syscall time rather than tracked separately, matching Linux's `get_sigframe()`/`do_sigaltstack()` semantics.
- **Hardening** — `rt_sigreturn` masks the restored `spsr_el1`/`rflags` value to just the condition-code bits before applying it, so a forged signal-stack frame can't be used to request a privilege escalation (e.g. an EL0→EL1 mode-bit forgery on AArch64) via `sigreturn`.
- **Testing** — `userland/sigtest` covers a `sigaction()` struct field-order round trip, real delivery-and-return through the sigreturn trampoline, per-signal handler dispatch, `sigprocmask`/`sigpending` blocking and deferred delivery, `SIG_IGN`, and `raise()` (which resolves to a `TKILL` syscall against the caller's own tid, distinct from the `KILL`/`TGKILL` paths the other checks exercise).

### POSIX timers

`timer_create`/`timer_settime`/`timer_gettime`/`timer_delete`/`timer_getoverrun`, `setitimer`/`getitimer`, and `alarm` are backed by a per-process timer table in `servers/tty`, checked on every syscall return; expiry delivers through the same signal path described above.

- **Handle encoding** — `timer_t` values are handed out as `slot + 1`, never the bare table index, since a raw `0` cast to a pointer is indistinguishable from `NULL` and relibc rejects a `NULL` `timer_t` as `EFAULT`.
- **Overrun accounting** — a timer descheduled across more than one period has its deadline caught up in a single step; the number of skipped periods accumulates in a per-timer counter read (and reset) by `timer_getoverrun()`.
- **`alarm()`/`setitimer(ITIMER_REAL)`** share one reserved, idempotently-rearmed table slot rather than allocating a fresh one per call, so repeated use can't exhaust the table.
- User-pointer access from the in-kernel `tty` server goes through `AddressSpace::read_user_buf`/`write_user_buf`, not a raw pointer dereference, for the same reason `wait`/`waitid`/`flock` do — a supervisor-mode page fault on a not-yet-faulted CoW page has no recovery path.

### Poll / select / epoll

`poll` and `select` are implemented in relibc's userspace on top of the kernel's `epoll_create1`/`epoll_ctl`/`epoll_pwait`. Readiness is queried from the object that owns each fd, never fabricated.

- **Real readiness** — the kernel routes each fd to a `VFS_POLL` (`servers/vfs`) or `NET_POLL` (`servers/net`) query that computes `POLLIN/POLLOUT/POLLERR/POLLHUP` from actual state (pipe ring occupancy and endpoint refcounts, eventfd counter, timerfd expiry, socket peer occupancy and liveness), then masks it against the requested event set. The pre-existing code reported whatever the caller *asked* for, unconditionally.
- **Honoured timeouts** — `ppoll`, `epoll_wait`, and `select` are cooperative retry loops that read the caller's deadline (`NULL` = block forever, `{0,0}` = single poll), yield between probes, and return `0` on true expiry.
- **`epoll_event` layout** — matches Linux exactly: 12-byte `#[repr(C, packed)]` (data at offset 4) on x86-64 only (`glibc EPOLL_PACKED`), natural 16-byte layout elsewhere. The kernel reads/writes the data word with arch-conditional offsets via `read_unaligned`/`write_unaligned`, so libc and kernel agree on both architectures.
- **Pipe endpoints are reference-counted** — a pipe read/write end held by several fds (via `dup`/`dup2` or inherited across `fork`) only signals EOF/`POLLHUP`/`EPIPE` once the *last* fd on that end closes, so `poll`/`select`/`epoll` stay correct for pipes shared across a `fork` (shell pipelines).

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

## Graphics stack

### KMS — Kernel Mode Setting

The KMS driver (`drivers/kms`) autodetects native display resolution via **EDID** and configures the framebuffer accordingly. It reads EDID blocks from the VirtIO-GPU device, parses detailed timing descriptors to extract preferred width, height, and refresh rate, then programs the hardware accordingly. This eliminates the need to hard-code display dimensions and allows the kernel to configure itself correctly on first boot across different display sizes.

### DRM subsystem

The DRM subsystem (`drivers/drm`, `servers/drm`) implements the Linux Direct Rendering Manager interface:

- **Device management** — CRTC, connector, encoder, and plane objects with full property trees.
- **Dumb buffer API** — `DRM_IOCTL_MODE_CREATE_DUMB` / `DRM_IOCTL_MODE_MAP_DUMB` for allocating and mapping scanout buffers from userspace.
- **Mode setting** — `DRM_IOCTL_MODE_SETCRTC` and `DRM_IOCTL_MODE_PAGE_FLIP` for display configuration and double-buffering.
- **Framebuffer objects** — `DRM_IOCTL_MODE_ADDFB` / `DRM_IOCTL_MODE_RMFB` for userspace-owned scanout surfaces.
- **Authentication** — DRM master tokens for secure multi-client access.
- **VirtIO-GPU IOCTLs** — `VIRTGPU_MAP`, `VIRTGPU_RESOURCE_CREATE`, `VIRTGPU_TRANSFER_TO_HOST`, `VIRTGPU_GET_CAPS`, and related operations for hardware-accelerated rendering via the VirtIO GPU protocol.
- **mmap** — `DRM_IOCTL_VIRTGPU_MAP` backed by `map_kernel_device` allows userspace to memory-map the hardware framebuffer directly, bypassing the VFS write path for maximum throughput.

The DRM server runs as a dedicated user-space task and exposes the device via the VFS as `/dev/dri/card0`. Userspace opens the device, authenticates, and then drives the display pipeline through standard Linux DRM ioctls, making it possible to run unmodified DRM client code.

### VirtIO GPU driver

The VirtIO GPU driver (`drivers/virtio_gpu`) implements the virtio-gpu 3D protocol over the PCI virtqueue transport:

- Full VirtIO PCI capability parsing with 64-bit BAR support.
- Control, cursor, and event virtqueues.
- 2D and 3D resource creation, transfer-to/from-host, and resource attachment to scanouts.
- Hardware-accelerated blits through the Virgl renderer when the host advertises 3D capability.

### Software scaling

The DRM subsystem implements **software scaling** (`drivers/drm`) allowing applications to render at a lower logical resolution and have the driver upscale to the physical display. Nearest-neighbour and bilinear modes are supported. This is used to run fixed-resolution content on high-DPI displays without layout changes in the application.

### Framebuffer console

The framebuffer console now renders text using a **Fira Code vector font** (`drivers/vector_font`). The driver parses TrueType/OpenType glyph outlines, rasterizes them at the requested point size, and caches rendered glyphs in a slab-backed glyph cache. The result is crisp, sub-pixel-positioned text in the kernel console at any resolution.

---

## Audio stack

### VirtIO Sound driver

The VirtIO Sound driver (`drivers/snd`) implements the `virtio-snd` specification over PCI:

- Control, event, TX, and RX virtqueues with a 256-entry ring.
- PCM stream lifecycle: `SET_PARAMS` → `PREPARE` → `START` → `STOP` → `RELEASE`.
- S16LE format at 44.1 kHz and 48 kHz, stereo.
- Non-blocking `send_pcm_data` that enqueues buffers into the TX virtqueue and returns the number of bytes accepted, allowing callers to back-pressure without blocking.
- Robust feature negotiation with timeouts to handle hosts that do not respond to all control commands.

### PipeWire server

The PipeWire server (`servers/pipewire`) sits between userspace audio clients and the VirtIO Sound driver:

- Registers itself as a VFS device at `/run/pipewire/pipewire-0` and handles IOCTLs.
- Maintains a 128 KiB **spooling buffer** between the client path and the hardware ring. Clients write audio data; the server drains the spool into the hardware in non-blocking chunks, decoupling client timing from hardware interrupt cadence.
- Port number is published via auxv at task spawn so clients can connect without a name-service lookup.

### `aplay`

`aplay` (`userland/aplay`) is a command-line audio player:

- Plays `.wav` files (PCM, 16-bit stereo) by streaming directly to the PipeWire server port.
- Plays `.mid` files via a built-in software synthesizer.
- Accepts `test` as a filename to generate and play a reference tone.
- Discovers the audio server port from the auxiliary vector via `get_audio_port()`.

---

## Input subsystem

### evdev server

The evdev server (`servers/evdev`) exposes keyboard and pointer hardware using the standard Linux `input_event` interface:

- Implements the `struct input_event { timeval, type, code, value }` layout from `linux/input.h`.
- Per-device event ring buffers (64 events × 4 devices).
- Supports `EV_KEY`, `EV_SYN`, and `EV_REL` event types.
- Devices are registered as VFS nodes (`/dev/input/event0`, etc.) and clients read events via standard `read` ioctls.
- Full Linux scan-code mapping (`KEY_*` constants) so that existing input libraries can be used without modification.
- Key down/up events are delivered separately with no synthetic repeat injection; repeat is left to the application layer.
- Shift-key state tracking and full printable character mapping including special symbols.

---

## C library

### relibc

LeandrOS ships **relibc** (external sibling checkout at `../relibc`, built by `scripts/build-all.sh`) — a full-featured C standard library originally developed for Redox OS — as its primary C runtime for userspace:

- Complete POSIX libc implementation in Rust, compiled to a `staticlib`.
- Covers `stdio`, `stdlib`, `string`, `math`, `pthread`, `signal`, `time`, `unistd`, `sys/mman`, `dlfcn`, and many more headers.
- Backed by `dlmalloc` for heap allocation and `openlibm` for `libm` functions.
- Includes `crt0`, `crti`, `crtn`, and a dynamic linker (`ld_so`) for position-independent executables.
- C programs compiled against relibc can use the full LeandrOS syscall ABI via the same `leandros-libc` shim layer without modification.
- Enables porting of existing C/C++ software with minimal changes.

### leandros-libc

The thin `leandros-libc` (`userland/libc`) provides Rust-callable wrappers around the LeandrOS syscall ABI: `open`, `read`, `write`, `close`, `mmap`, `ipc_call`, `get_audio_port`, and port-discovery helpers exported with C linkage so that relibc and native Rust userland can share the same interface.

### Threading (pthreads)

Full pthread support — `pthread_create`/`join`, mutexes, condition variables, thread-local storage, and cleanup handlers — is verified working on both architectures.

Following the same split real Linux libcs use, only a handful of generic primitives live in the kernel; everything POSIX-shaped is built on top of them in userspace:

- **Kernel primitives** — `clone` (the `CLONE_VM` branch spawns a true OS thread sharing the caller's address space, distinct from the `CLONE_VM`-clear `fork` path) and `futex` (`FUTEX_WAIT`/`FUTEX_WAIT_BITSET` and `FUTEX_WAKE`) are the only thread-related kernel syscalls.
- **Userspace (relibc)** — `RlctMutex`/`RlctCondvar` (built on `futex`), a `#[thread_local]` TSD map with destructor-on-exit support, and `pthread_cleanup_push`/`pop` all live in `userland/relibc`, the same layering glibc and musl use on real Linux. The kernel has no mutex, condvar, or TSD syscalls of its own.
- Both `x86_64` and `aarch64` implement the raw thread-spawn path (`rlct_clone`) that `pthread_create` calls into; a contended mutex, condvar wait/signal, and TSD destructor ordering have all been exercised in QEMU on both targets.

---

## Server layer

All drivers that need userspace access are fronted by **server tasks** that accept IPC messages and dispatch to the underlying driver:

| Server | VFS path | Protocol |
|---|---|---|
| `vfs` | — | File descriptor table, `open`/`read`/`write`/`close`/`ioctl` routing |
| `drm` | `/dev/dri/card0` | Linux DRM ioctls over VFS ioctl messages |
| `evdev` | `/dev/input/eventN` | Linux `input_event` reads |
| `pipewire` | `/run/pipewire/pipewire-0` | PCM write, IOCTL control |
| `tty` | `/dev/tty0` | Terminal line discipline |
| `proc` | `/proc` | Process information |
| `net` | `/dev/net/tun` | Network I/O |

VFS resource lifecycle (open/close notifications) ensures that server-side handles are cleaned up when a client task exits or closes a descriptor.

---

## Key design decisions

**No `unsafe` globals beyond boot parsers** — static mutable state is wrapped in `spin::Mutex` throughout the kernel. The only bare `static mut` blocks are in the boot parsers (run single-threaded before any AP starts) and the arch assembly stubs.

**W^X enforced** — `sys_map_mem` rejects any mapping with both `WRITABLE` and `EXECUTE` flags set.

**SMAP** — the kernel uses architecture supervisor-mode access prevention so that kernel code cannot accidentally dereference userspace pointers; all cross-boundary copies go through explicit checked accessors.

**Checked arithmetic at memory boundaries** — VMA end-address calculations, slab order arithmetic, and all DTB offset reads use `checked_add` rather than wrapping arithmetic.

**Demand paging** — user tasks call `sys_map_mem` with a lazy flag to reserve virtual address space without touching physical memory. Each page is allocated and mapped on first access via the page-fault handler.

**Per-CPU SYSCALL stacks (x86-64)** — each CPU has a `PerCpuSyscall { kernel_stack_top, user_rsp_save }` struct pointed at by `IA32_KERNEL_GS_BASE`. The syscall entry stub uses `swapgs` + `%gs:0/8` to load the kernel stack and save the user RSP without touching any shared state.

**Auxv service discovery** — rather than a name-service daemon, the kernel stamps server port IDs directly into the ELF auxiliary vector of each spawned task. `get_audio_port()`, `get_drm_port()`, etc. read these at startup in O(1).

**Asynchronous audio pipeline** — the PipeWire spool buffer decouples audio client timing from the VirtIO hardware ring. Clients never block on a full ring; the server drains the spool at hardware interrupt cadence, preventing audio starvation and eliminating client freezes.

**Non-blocking DRM mmap** — userspace can map the hardware framebuffer directly via `DRM_IOCTL_VIRTGPU_MAP` backed by `map_kernel_device`. This bypasses the VFS write path entirely for display updates, achieving the throughput needed for full-frame rendering.

**Exit-log side-table** — `sched::run()` reaps zombie tasks immediately after they exit, but records their exit code in a 256-slot `EXIT_LOG` table keyed by PID. `wait_pid` falls back to this table when `find_pid` returns `None`, eliminating the race between the reaper and the waiter.

---

## Userland programs

| Program | Description |
|---|---|
| `init` | PID-1 process; spawns servers and user tasks from the initrd |
| `shell` | Interactive CLI with Unix-style commands and VFS integration |
| `aplay` | Command-line audio player (WAV, MIDI, test tone) |
| `hello` | Minimal "Hello, world!" demonstration |
| `memtest` | Regression suite: fork/CoW isolation, `mremap`, buddy allocator churn, `MAP_SHARED` |
| `vfstest` | Regression suite: `rmdir`, cross-mount `rename`, `flock`/`fcntl` locking, permissions |
| `f2fstest` | Regression suite: F2FS direct/indirect/double-indirect block pointers, directories |
| `pthreadtest` | Regression suite: `pthread_create`/`join`, mutex, condvar, TSD, cleanup handlers |
| `timertest` | Regression suite: POSIX timers, `alarm`/`setitimer`, real `SIGALRM` delivery |
| `sigtest` | Regression suite: `sigaction` struct layout, signal delivery/return, `sigprocmask`/`sigpending`, `SIG_IGN`, `raise()` |
| `polltest` | Regression suite: `poll`/`select`/`epoll` real fd readiness, `epoll_wait` timeout, pipe `POLLHUP` writer refcount across `dup` |

Programs are linked against **relibc** for full POSIX compatibility, or against the lighter `leandros-libc` shim for `no_std` Rust programs. Binaries are embedded in the initrd image and extracted at boot. `pthreadtest`, `timertest`, `sigtest`, and `polltest` link `librelibc.a` directly rather than `leandros-libc`, since they need TLS bring-up (`relibc_start_v1`) for real `pthread`/`errno`/`sigaction`/`epoll` support.

---

## License

GPL-3.0
