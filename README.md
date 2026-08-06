# Leandros

A `no_std` bare-metal microkernel written in Rust, targeting **x86-64** (UEFI/QEMU) and **AArch64** (QEMU virt, Raspberry Pi 5).

Leandros follows the classic microkernel design: the kernel itself provides only scheduling, IPC, and memory management. Everything else — drivers, file systems, network stacks — runs as isolated user-space tasks that communicate via typed message passing.

On top of that microkernel core sits a **Linux-compatible syscall personality**: the dispatcher implements the real Linux syscall numbers, so unmodified `*-unknown-linux-musl` binaries — a shell, coreutils, Mesa, a Wayland compositor — run without a source patch or a shim ABI.

**Current state** — Leandros boots to a login prompt on both architectures and runs the **COSMIC desktop environment** unmodified: `cosmic-session` → `cosmic-comp` on KMS/softpipe → `busd` (D-Bus) → `cosmic-bg` + `cosmic-panel`, rendering a wallpaper plus a full-width panel bar with an embedded Wayland client. Vulkan reaches real host GPU hardware through Mesa's **Venus** ICD over virtio-gpu.

---

## Architecture at a glance

```
┌──────────────────────────────────────────────────────────┐
│                     User Space (EL0 / Ring 3)            │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
│  │  COSMIC  │  │  brush   │  │  aplay   │  │  other   │  │
│  │ desktop  │  │  shell   │  │ WAV/MIDI │  │  tasks   │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  │
│  ┌────┴─────────────┴─────────────┴─────────────┴─────┐  │
│  │  Mesa (softpipe / Venus) · libinput · relibc/musl  │  │
│  └────┬─────────────┬─────────────┬─────────────┬─────┘  │
│       │   SYSCALL   │             │             │        │
├───────┼─────────────┼─────────────┼─────────────┼────────┤
│                 Server Layer (EL0 / Ring 3)              │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
│  │   VFS    │  │   DRM    │  │ PipeWire │  │  evdev   │  │
│  │  server  │  │  server  │  │  server  │  │  server  │  │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
│  │   F2FS   │  │   net    │  │   tty    │  │  xattr   │  │
│  │  server  │  │ (smoltcp)│  │  server  │  │ + ACLs   │  │
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
| `capability` | Capability handle types passed in message slots |
| `elf` | ELF64 parser and loader; `ET_EXEC` + `ET_DYN`-at-a-bias, `PT_INTERP` |
| `boot` | Multiboot2, Limine, and Device Tree (FDT) parsers → `BootInfo` |
| `arch/x86_64` | GDT/TSS, IDT, APIC, PIC, SYSCALL entry, SMP, timer |
| `arch/aarch64` | MMU, exception vectors, GICv2, generic timer, UART, SMP/PSCI, debug utils |
| `drivers` | KMS, DRM subsystem, VirtIO GPU/Sound/Block/Net/Input, framebuffer, serial, PCI, SDHCI |
| `drivers/usb` | xHCI host controller |
| `drivers/wifi` | mac80211 + virtio-wifi |
| `servers/vfs` | Virtual filesystem server: fd tables, tmpfs, mounts, pipes, eventfd/timerfd/signalfd/inotify |
| `servers/f2fs` | F2FS filesystem server — real persistent storage on virtio-blk |
| `servers/xattr` | Shared xattr + POSIX ACL contract (codec, namespace gates, ACL evaluator) |
| `servers/drm` | DRM server (hardware-accelerated graphics IPC) |
| `servers/evdev` | Linux-compatible input event server |
| `servers/pipewire` | PipeWire-compatible audio server |
| `servers/tty` | TTY server, job control, POSIX timers |
| `servers/net` | Network server — smoltcp TCP/IP plus a full AF_UNIX implementation |
| `servers/proc` | `/proc` filesystem |
| `servers/init` | PID-1: server bring-up, mounts, getty loop |
| `servers/libc-shim` | In-kernel glue backing the userspace libc |
| `userland` | User-space programs and regression suites (leandros-libc / relibc / musl) |
| `ports` | Build recipes for third-party software: Mesa, D-Bus/busd, COSMIC, input shims |
| `lib` | `align_up` / `align_down` utilities shared across crates |

---

## Supported targets

| Target | Boot protocol | Status |
|---|---|---|
| x86-64 (QEMU q35) | Limine UEFI | Working |
| AArch64 (QEMU virt) | Limine UEFI (default), or DTB via `-kernel` (`--direct`) | Working |
| AArch64 (QEMU raspi4b) | BCM2711 board model, `--raspi4b` | SDHCI driver test path only |
| Raspberry Pi 5 | RPi firmware ELF load + BCM2712 DTB | Boot-ready |

Every change must be tested on **both** architectures — cross-platform parity is a project invariant, not a nice-to-have.

### Host platforms

Both macOS and Linux hosts build and boot the full system. `run-qemu.sh` picks the fastest accelerator the host can actually provide for the requested guest:

| Host | Guest | Accelerator |
|---|---|---|
| macOS (Apple Silicon) | aarch64, UEFI boot | HVF |
| Linux, `/dev/kvm` writable | matching arch | KVM |
| anything else (incl. cross-arch) | any | TCG software emulation |

A hypervisor virtualises, it does not translate — so a mismatched host/guest arch always falls back to TCG, which is *much* slower (a COSMIC session on TCG needs several minutes to settle). Override with `--hvf`, `--kvm`, or `--tcg`.

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

### Sibling repositories

Some large dependencies live *outside* this tree, as siblings of the repo root, and are built by `scripts/build-all.sh`:

| Path | Purpose |
|---|---|
| `../relibc` | The C standard library shipped as the userspace libc |
| `../doomgeneric` | Doom port |
| `../cosmic-epoch` | COSMIC desktop sources (built unmodified) |
| `../brush` | The `brush` shell |

### musl cross toolchain

Third-party userspace (Mesa, COSMIC, brush, coreutils) is built for `{x86_64,aarch64}-unknown-linux-musl` and dynamically linked against a real `ld-musl`. The `scripts/cc-*-musl.sh`, `linker-*-musl.sh`, and `ar-musl.sh` wrappers drive that cross-build; the build scripts pass the pinned nightly toolchain (`rust-toolchain.toml`) through to sibling-repo builds so every crate compiles against one compiler version.

---

## Building

Use the top-level build script to compile all targets:

```sh
./scripts/build-all.sh                       # both architectures
./scripts/build-all.sh --arch aarch64        # one architecture
./scripts/build-all.sh --arch aarch64 --rpi5 # Raspberry Pi 5 features
./scripts/build-all.sh --raspi4b             # QEMU raspi4b / SDHCI test path
```

⚠️ **Important**: Always use release builds — debug builds may hang during early boot due to large stack requirements and symbol desync issues.

---

## Running in QEMU

```sh
# Test both architectures
./scripts/run-qemu.sh aarch64
./scripts/run-qemu.sh x86_64

# Boot-mode and accelerator overrides
./scripts/run-qemu.sh aarch64 --direct    # DTB via -kernel, no bootloader
./scripts/run-qemu.sh aarch64 --tcg       # force software emulation
./scripts/run-qemu.sh --raspi4b           # BCM2711 board model (aarch64 only)
```

Both architectures boot via **Limine UEFI** by default: the runner builds a fresh FAT32 disk image containing Limine (rev ≥ 6) and the kernel ELF, then launches QEMU with OVMF/AAVMF. `--direct` on AArch64 instead boots the ELF with `-kernel`, passing the virt machine's built-in DTB in `x0`.

The runner also handles the host environment for you:

- **F2FS data disks** — `f2fs-data{0,1}-<arch>.img` (64 MB each) are created once on first run and reused. Disk 0 is populated by `scripts/mkfs-f2fs-populated.py` with the full userland: shell, coreutils, test binaries, Mesa, and the COSMIC session set. Delete an image to force a clean rebuild.
- **Networking** — `socket_vmnet` on macOS when its daemon is running (routable guest at `192.168.105.2`), otherwise user-mode SLIRP (`10.0.2.15` behind NAT). Either way the guest configures itself over DHCP via `servers/net`'s smoltcp client, so nothing in the guest needs to know which it got.
- **Display and audio** — a headless host (an SSH build box with no `$DISPLAY`/`$WAYLAND_DISPLAY`) gets `egl-headless`, preserving GL for Venus, or `-display none` if GL is unavailable. The audio backend is probed rather than assumed, because QEMU *aborts at startup* if the requested backend cannot open.

### Logging in

Boot lands on a login prompt served by a getty loop in PID-1. Two accounts are provisioned in `/etc/shadow` (SHA-256 crypt):

| User | Password |
|---|---|
| `root` | `root` |
| `leandro` | `leandro` |

Non-root sessions are fully supported, including a COSMIC desktop session with per-uid `/run/user/<uid>` runtime directories.

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
- SMP: up to 8 CPUs. BSP runs `sched::run()`; APs are started via PSCI `CPU_ON` (AArch64) and SIPI (x86-64), then enter `sched::ap_entry()`. Idle-CPU selection is SMT-aware on x86-64 (hyperthread topology from CPUID leaf 0xB).
- `wait_pid` uses an exit-log side-table to avoid the race where the scheduler reaps a zombie before the waiter resumes.
- **Threads** — `clone` with `CLONE_VM` spawns a true OS thread sharing the caller's address space; `futex` (`FUTEX_WAIT`/`FUTEX_WAIT_BITSET`/`FUTEX_WAKE`) is the only other thread primitive in the kernel. Timed futex waiters are woken by a cross-thread `FUTEX_WAKE`, not left to time out.
- **A global poll wait-channel with a deadline tick** lets blocking waiters sleep instead of spinning; servers publish readiness edges onto it.
- User stacks are 8 MB, matching the Linux default.
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

The table above is only the original minimal IPC/memory surface. The dispatcher (`kernel/src/syscall.rs`, ~7.5k lines) has since grown into a **Linux syscall personality**: `clone`, `futex`, `rt_sigaction`, the `mmap` family, the full VFS surface, sockets, `epoll`, xattrs, `chroot`, `setresuid`, `faccessat2`, `mincore`, and the timer calls all use their *actual Linux numbers* rather than a custom scheme. Consequences:

- relibc's `platform::linux` Pal implementation works unmodified.
- So does a stock musl binary cross-compiled for `*-unknown-linux-musl` — which is how Mesa, D-Bus, and COSMIC run here without source patches.
- The two architectures have genuinely different syscall numbers for the same call (e.g. `mincore` is 232 on AArch64 and 27 on x86-64); the tables are per-arch, and getting one backwards is a class of bug this project has paid for more than once.

Beyond the syscall surface, the kernel also stamps server port IDs into the ELF auxiliary vector, so a task discovers the audio/DRM/VFS servers in O(1) without a name-service round-trip.

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
- **Blocking, not spinning** — waiters park on a global poll wait-channel with a deadline tick; servers publish readiness *edges* onto that channel. `wait4`/`waitid`/`nanosleep` and the net daemon block rather than busy-polling, so an idle system is genuinely idle.
- `signalfd4`, `inotify`, and `eventfd`/`timerfd` honour `O_NONBLOCK` and `*_CLOEXEC` at creation; pool slots are refcounted so `dup`'d fds do not alias each other.

### Processes, users, and sessions

The process model is POSIX-shaped rather than task-shaped, which is what allows a real shell and a real desktop session to run:

- **Thread groups** — a process is a TGID with threads under it. A fatal signal or a fatal user fault terminates the **whole thread group**, not the one thread that took it; `execve` de-threads (terminates siblings) and resets caught signal dispositions, both per POSIX. Per-process tables are keyed by TGID, never by the raw pid of whichever thread happened to call.
- **Login sessions** — real `setresuid`/`setresgid`, a `$sha256$`-hashed `/etc/shadow`, `/bin/login`, and a getty loop in PID-1. Sessions run as an unprivileged user with correct ownership throughout.
- **Job control** — `servers/tty` implements the job-control ioctls and line-discipline signal generation (`SIGINT`/`SIGTSTP`/`SIGTTOU`), so shell job control behaves.
- **`chroot(2)`** — real confinement: symlink resolution and fd-to-path resolution are both confined to the jail rather than escaping through the mount table.
- **Priorities** — `setpriority`/`getpriority` back `nice(1)`.
- **Process state** — `/proc` via `servers/proc`, including `/proc/self/exe`.

### Filesystems and storage

| Filesystem | Where | Backing |
|---|---|---|
| `f2fs` | `/`, `/data` | virtio-blk, real persistent storage |
| `tmpfs` | `/tmp`, `/run`, `/run/user/<uid>` | Anonymous memory, page-shared across processes |
| `procfs` | `/proc` | Synthesized |
| devices | `/dev/*` | Server-backed nodes |

**F2FS** (`servers/f2fs`) is a real read-write implementation, not a stub: direct/indirect/double-indirect block pointers, multi-block directories, hardlinks and symlinks, ownership and mode persistence with enforced `chmod`/`chown`, block reclamation on `unlink`/`rmdir`/truncate, `statfs` computed from live SIT state, synchronous checkpointing of namespace mutations, and POSIX `rename` that atomically replaces its destination. `fsync`/`sync` actually flush to the device.

**Extended attributes and POSIX ACLs** are enforced end to end. `servers/xattr` is the single source of truth — the size caps, namespace indices, packed entry codec, namespace permission gates, and the POSIX 1003.1e ACL evaluator are shared verbatim by the kernel syscall layer, tmpfs, and f2fs, so an xattr blob written by one is interpretable by the other. The on-disk arena uses the Linux `f2fs_xattr_entry` layout.

**Shared memory** — tmpfs and memfd pages are shared across processes via frame-backed promotion, with `memfd` seal lifecycle honoured. This is load-bearing for Wayland: `wl_shm` pools *are* shared memfd mappings.

Other VFS behaviour worth knowing: per-process fd tables (limit 128), one identity for the console so `isatty` stops lying, arch-correct `stat`, `O_NOFOLLOW`/`O_DIRECTORY`/`AT_SYMLINK_NOFOLLOW`, `mount`/`umount`/`fstab`, and `fstat` proxied to the owning mount with refcounted dup'd mounted fds.

### Networking

`servers/net` provides two largely separate stacks:

- **TCP/IP over virtio-net**, via smoltcp, with a DHCPv4 client. Host↔VM networking works in both directions on both accelerators; `ping` is in the userland.
- **AF_UNIX**, implemented in full — because every Wayland and D-Bus connection in the system rides on it. `SCM_RIGHTS` fd passing, cmsg flags, `SO_PEERCRED`, socket nodes bound to real VFS pathnames, `accept4()` honouring `SOCK_NONBLOCK`/`SOCK_CLOEXEC`, `F_SETFD` cloexec on socket fds, `FIONBIO`, well-formed peer sockaddrs from `accept()`, writes buffered on connected-but-not-yet-accepted stream sockets, connections kept alive until the last fd reference closes, and a full send ring correctly returning `EAGAIN` rather than a bogus `0`.

That last one is worth calling out: a stream `send` that returns `0` instead of `EAGAIN` is indistinguishable from a zero-length write to a client, and it desynchronised cosmic-panel's Wayland object stream — a bug that presented as an inexplicable `Unknown id` protocol error many layers above the actual fault.

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

**Userspace hand-off** (both architectures)

```
init (PID 1) → start servers (vfs, f2fs, net, tty, drm, evdev, pipewire, proc)
             → mount / and /data from virtio-blk
             → getty loop → /bin/login → authenticate against /etc/shadow
             → setresuid/setresgid → brush
             → (optionally) start-cosmic → cosmic-session → COSMIC desktop
```

**Boot protocol invariant** — the minimum Limine revision is **6**. Do not lower it.

---

## Graphics stack

### KMS — Kernel Mode Setting

The KMS driver (`drivers/kms`) autodetects native display resolution via **EDID** and configures the framebuffer accordingly. It reads EDID blocks from the VirtIO-GPU device, parses detailed timing descriptors to extract preferred width, height, and refresh rate, then programs the hardware accordingly. This eliminates the need to hard-code display dimensions and allows the kernel to configure itself correctly on first boot across different display sizes.

### DRM subsystem

The DRM subsystem (`drivers/drm`, `servers/drm`) implements the Linux Direct Rendering Manager interface:

- **Device management** — CRTC, connector, encoder, and plane objects with full property trees, `OBJ_GETPROPERTIES`, and valid connector-mode timings.
- **Dumb buffer API** — `DRM_IOCTL_MODE_CREATE_DUMB` / `DRM_IOCTL_MODE_MAP_DUMB` for allocating and mapping scanout buffers from userspace.
- **Legacy mode setting** — `DRM_IOCTL_MODE_SETCRTC` and `DRM_IOCTL_MODE_PAGE_FLIP` for display configuration and double-buffering. `SETCRTC` presents the framebuffer immediately, so a mode-setting compositor's frame 0 actually appears.
- **Atomic KMS** — `DRM_CLIENT_CAP_ATOMIC` and `DRM_IOCTL_MODE_ATOMIC`, including `TEST_ONLY` and `ALLOW_MODESET`, with the legacy handlers still live for clients that do not opt in. This is the preferred path; the legacy path cannot drive a cursor plane.
- **Planes** — a primary plane plus a real **cursor plane**, driven from the virtio-gpu cursor queue. Before the cursor plane existed, every pointer movement forced a full-screen software recomposite; moving the pointer now costs a cursor-queue update instead of a repaint.
- **Framebuffer objects** — `DRM_IOCTL_MODE_ADDFB` / `DRM_IOCTL_MODE_RMFB` for userspace-owned scanout surfaces.
- **PRIME / dmabuf** — `PRIME_HANDLE_TO_FD` / `FD_TO_HANDLE` via borrowed dumb-buffer VMOs, with `DRM_CAP_PRIME` reported so GBM's softpipe backend takes the DRIimage path. Correct for multithreaded clients, and the ephemeral export node is unlinked so it cannot leak the tmpfs pool.
- **Authentication** — DRM master tokens for secure multi-client access.
- **VirtIO-GPU IOCTLs** — `VIRTGPU_MAP`, `VIRTGPU_RESOURCE_CREATE`, `VIRTGPU_TRANSFER_TO_HOST`, `VIRTGPU_GET_CAPS`, `VIRTGPU_EXECBUFFER`, and related operations.
- **mmap** — `DRM_IOCTL_VIRTGPU_MAP` backed by `map_kernel_device` allows userspace to memory-map the hardware framebuffer directly, bypassing the VFS write path for maximum throughput.

The DRM server runs as a dedicated user-space task and exposes the device via the VFS as `/dev/dri/card0`, plus a **render node** at `/dev/dri/renderD128`. Userspace opens the device, authenticates, and then drives the display pipeline through standard Linux DRM ioctls, making it possible to run unmodified DRM client code — `kmscube` renders animated frames on both architectures, and `cosmic-comp` drives the display through this path.

State that upstream scopes per open-file is scoped per open-file here too: the virtio-gpu 3D context, GEM handle ownership, and fences are keyed off the open file description rather than the process, which is what allows a multithreaded compositor and a Vulkan client to hold the device at the same time without stepping on each other.

### VirtIO GPU driver

The VirtIO GPU driver (`drivers/virtio_gpu`) implements the virtio-gpu 3D protocol over the PCI virtqueue transport:

- Full VirtIO PCI capability parsing with 64-bit BAR support.
- Control, cursor, and event virtqueues.
- 2D and 3D resource creation, transfer-to/from-host, and resource attachment to scanouts.
- Hardware-accelerated blits through the Virgl renderer when the host advertises 3D capability.

### Software scaling

The DRM subsystem implements **software scaling** (`drivers/drm`) allowing applications to render at a lower logical resolution and have the driver upscale to the physical display. Nearest-neighbour and bilinear modes are supported. This is used to run fixed-resolution content on high-DPI displays without layout changes in the application.

### Framebuffer console

The framebuffer console renders text using a **Fira Code vector font** (`drivers/vector_font`). The driver parses TrueType/OpenType glyph outlines, rasterizes them at the requested point size, and caches rendered glyphs in a slab-backed glyph cache. The result is crisp, sub-pixel-positioned text in the kernel console at any resolution.

The console is a real **VT emulator**, not a print sink: it answers cursor-position reports (`ESC[6n`), reports the true console geometry through `TIOCGWINSZ`, and delivers multi-byte ANSI escape sequences to a reader in one `read` rather than splitting them. The framebuffer is the authoritative console — it wins geometry ties against the serial port.

Console writes are **atomic**. They were not always: a kernel trace interleaved mid-sequence with the shell's `ESC[38;5;` prompt colour once spliced together into a literal `ESC[3i` — ANSI *media copy* — which opened a print dialog on the host terminal. The CSI final-byte alphabet is now restricted to what the console actually implements.

---

## GPU acceleration — Vulkan via Venus

`vktest` running inside LeandrOS enumerates and opens a **real host GPU** through Mesa's Venus ICD, on a stock host with no custom virglrenderer:

```
vkEnumeratePhysicalDevices count=1
  "Virtio-GPU Venus (AMD Ryzen 9 7950X (RADV RAPHAEL_MENDOCINO))"
vkCreateDevice VK_SUCCESS
```

The path is: guest Vulkan client → Mesa Venus ICD → `VIRTGPU_EXECBUFFER` on our render node → virtio-gpu virtqueue → host virglrenderer's `vkr` decoder → host GPU driver.

The kernel-side pieces that make this work:

- A host-populated **Venus capset** returned from `VIRTGPU_GET_CAPS`.
- A **render node** (`/dev/dri/renderD128`) that is discoverable — `opendir("/dev/dri")` enumerates it and `drmGetVersion` passes Mesa's identity check.
- Correct 3D wire protocol: `CTX_ATTACH_RESOURCE`, honoured `ring_idx`, and hard failures instead of silent ones.
- **HOST3D blob mapping** through the host-visible window, which is how Mesa's command ring is shared with the host.
- `GETPARAM` writing *through* the user pointer, as upstream does — the value field is a pointer, not a scalar.
- Device VMAs **aliased across `fork`** rather than copied.

Required QEMU device line (the runner selects this automatically when available):

```
-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless
```

⚠️ `-nographic` silently overrides `-display`, and this needs a host EGL implementation — so the Venus path requires a **Linux host**; macOS has no EGL.

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
- Supports `EV_KEY`, `EV_SYN`, `EV_REL`, and `EV_ABS` event types.
- Devices are registered as VFS nodes (`/dev/input/event0`, etc.) and clients read events via standard `read` ioctls.
- **virtio-keyboard** (`event0`) and **virtio-tablet** (`event1`, an absolute-position pointer reporting `INPUT_PROP_POINTER`) are attached by default on both architectures, with the full `EVIOC*` ioctl surface libinput queries during device probe.
- Every virtio-input function on the bus is bound and routed to evdev, rather than only the first.
- Full Linux scan-code mapping (`KEY_*` constants) so that existing input libraries can be used without modification.
- Key down/up events are delivered separately with no synthetic repeat injection; repeat is left to the application layer.
- Shift-key state tracking and full printable character mapping including special symbols.

### Seat and device discovery

Real **libinput** and **libxkbcommon** are ported and run unmodified. The two pieces of Linux plumbing beneath them that assume a daemon we do not have — `libseat` (seatd) and `libudev` (udevd) — are replaced by **shims** in `ports/input-stack`, backed by a synthetic sysfs skeleton. There is no seatd, no udevd, and no VT switching.

---

## Desktop stack — Wayland and COSMIC

The **COSMIC desktop environment runs unmodified** on both architectures: no source patches, only build-configuration flags. Everything beneath it is ours.

```
cosmic-session
  ├─ busd                D-Bus broker (pure Rust, from the zbus authors)
  ├─ cosmic-comp         Wayland compositor, on KMS via Mesa softpipe
  ├─ cosmic-bg           wallpaper
  └─ cosmic-panel        panel bar + embedded Wayland applet clients
```

**Committed architecture.** COSMIC is built for `*-unknown-linux-musl` and **dynamically linked** against a real `ld-musl` — `dlopen` sits on the critical path in three separate places (cosmic-comp's EGL loading, Mesa's GBM/DRI loader, cosmic-panel's `use_system_lib`), so static linking was never an option. Graphics go through Mesa **softpipe** via gallium `kms_swrast` over dumb buffers, on the atomic KMS path. The session launches from a login shell: login → `start-cosmic` under `brush`.

Supporting this required the ELF loader to grow real dynamic-linking support — `ET_DYN` loaded at a bias, `PT_INTERP` honoured — plus VMA splitting at range boundaries for `munmap`/`mprotect`, an 8 MB user stack matching the Linux default, and shared tmpfs/memfd pages for `wl_shm` pools.

Most of the kernel work in this area was not "add a feature" but "match Linux exactly": a full multithreaded GPU desktop is an extremely effective probe for compatibility gaps that no test suite had surfaced. Roughly thirty real kernel bugs were found and fixed this way — among them the `execve` signal-reset/de-thread trio, TGID-keying of per-process tables, dropped `eventfd`/`accept4`/`socketpair`/`F_SETFD` flags, a poll-deadline lost-wake, a cross-thread timed-futex wake race, `AF_UNIX` refcounting and full-ring `EAGAIN`, and a `mincore` stub that returned success for unmapped pages (which made Mesa's `_eglPointerIsDereferenceable` accept `(void*)3` and fault the panel deep inside `eglCreatePlatformWindowSurfaceEXT`).

### Ports

`ports/` holds the build recipes for third-party software, each with the exact toolchain invocation that produces a working binary:

| Port | Contents |
|---|---|
| `mesa` | softpipe / `kms_swrast` GL stack, and the Venus Vulkan ICD |
| `gl-stack` | GL support libraries and test clients (`kmscube`) |
| `input-stack` | libinput, libxkbcommon, and the libseat / libudev shims |
| `dbus`, `busd` | Reference `dbus-daemon`, and the `busd` broker actually used |
| `cosmic-session`, `cosmic-greeter` | Session and greeter build configuration |

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

### musl

Ported third-party software — Mesa, D-Bus/busd, COSMIC, brush, coreutils — is built for `*-unknown-linux-musl` and **dynamically linked** against a real `ld-musl`, which is shipped in the image. This is not a second-class path: it is how the desktop runs, and it works precisely because the kernel implements the real Linux syscall numbers rather than a translation layer. Nothing in those projects is patched to know it is running on LeandrOS.

One toolchain caveat worth recording: musl static binaries must be built with `-C relocation-model=static`. Left to its own devices, `x86_64` defaults to static-PIE, which the loader maps at vaddr 0.

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
| `vfs` | — | File descriptor table, `open`/`read`/`write`/`close`/`ioctl` routing, tmpfs, mounts |
| `f2fs` | `/`, `/data` | On-disk filesystem over virtio-blk |
| `drm` | `/dev/dri/card0`, `/dev/dri/renderD128` | Linux DRM ioctls over VFS ioctl messages |
| `evdev` | `/dev/input/eventN` | Linux `input_event` reads |
| `pipewire` | `/run/pipewire/pipewire-0` | PCM write, IOCTL control |
| `tty` | `/dev/tty0` | Terminal line discipline, job control, POSIX timers |
| `proc` | `/proc` | Process information |
| `net` | AF_UNIX socket nodes, `/dev/net/tun` | Sockets, TCP/IP, AF_UNIX |

VFS resource lifecycle (open/close notifications) ensures that server-side handles are cleaned up when a client task exits or closes a descriptor — on *every* path out of a task, including a fatal signal, not just a clean `exit`.

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

**Never touch user memory under a scheduler spinlock** — a demand-paging fault taken while holding `RUN_QUEUE` with interrupts off re-enters the scheduler lock and freezes every vCPU with no panic and no output. All cross-boundary copies in the scheduler and the in-kernel servers go through `AddressSpace::read_user_buf`/`write_user_buf` outside the lock, for `sigprocmask`/`sigaction`, `wait`/`waitid`/`flock`, and the timer tables alike.

**One resolver for every path syscall** — path resolution, symlink following, `AT_*` dirfd handling, and chroot confinement live in a single resolver rather than being reimplemented per syscall. `renameat` honouring its dirfds, not just its paths, was the difference between a clean desktop session log and a storm of `ENOENT`s from atomic state writes.

**Debug output is gated and off by default** — render and audio hot paths, task lifecycle, `[MMAP]`/`[CPIO]` tracing, EL0 fault backtraces, and the unix-socket exchange trace are all behind consts defaulting to off. Unconditional serial writes on a hot path do not merely slow the system down; they change its timing enough to hide the race being investigated.

**Round up, never truncate, when converting time** — `nanosleep` truncating every sub-tick sleep to zero passed every correctness test in the tree and was invisible for months, until Mesa's watchdog fired 200× early and `abort()`ed. `nanosleep` rounds up and `clock_nanosleep` honours `TIMER_ABSTIME`.

---

## Userland programs

### Programs

| Program | Description |
|---|---|
| `init` | PID-1 process; brings up servers, mounts filesystems, runs the getty loop |
| `login` | Authenticates against `/etc/shadow`, drops privilege via `setresuid`/`setresgid` |
| `shell` | Built-in CLI with Unix-style commands and VFS integration |
| `brush` | Ported POSIX shell — the interactive default, with working job control |
| `aplay` | Command-line audio player (WAV, MIDI, test tone) |
| `mount` / `umount` / `fstab` | Filesystem mount management |
| `lsblk` / `lspci` / `lsusb` | Device enumeration with real block-device capacity |
| `ping` | ICMP echo over the smoltcp stack |
| `xattr-util` | Get/set/list extended attributes and POSIX ACLs |
| `tput` | Terminal capability query |
| `hello` | Minimal "Hello, world!" demonstration |

A ported **coreutils** provides the usual file and text utilities.

### Regression suites

| Suite | Coverage |
|---|---|
| `memtest` | fork/CoW isolation, `mremap`, buddy allocator churn, `MAP_SHARED` |
| `vfstest` | `rmdir`, cross-mount `rename` (incl. POSIX replace), `flock`/`fcntl` locking, permissions, xattrs |
| `f2fstest` | Direct/indirect/double-indirect block pointers, directories |
| `pthreadtest` | `pthread_create`/`join`, mutex, condvar, TSD, cleanup handlers |
| `timertest` | POSIX timers, `alarm`/`setitimer`, real `SIGALRM` delivery |
| `sigtest` | `sigaction` struct layout, signal delivery/return, `sigprocmask`/`sigpending`, `SIG_IGN`, `raise()` |
| `polltest` | `poll`/`select`/`epoll` real fd readiness, `epoll_wait` timeout, pipe `POLLHUP` writer refcount across `dup` |
| `epolltest` / `wakepolltest` | epoll edge cases; same-thread `EPOLLET`/level eventfd re-arm |
| `scmtest` | `SCM_RIGHTS` fd passing, shared mmap, memfd seals and collisions, AF_UNIX socket nodes, tmpfs mounts, `mincore`, fork+exec fd inheritance |
| `forktest` / `waittest` / `sigchldtest` / `racetest` | Process lifecycle, `wait4`/`waitid`, `SIGCHLD`, concurrency races |
| `idletest` | Verifies the system genuinely idles rather than busy-polling |
| `drmsmoke` | Raw-ioctl DRM smoke test against `/dev/dri/card0` |
| `evtest2` | evdev virtio-tablet capability and poll test |
| `venustest` | Venus/virtio-gpu 3D **transport** conformance — asserts a non-empty host-populated capset, since "the ioctl returned 0" proves nothing when the risk is a silently wrong wire protocol |

`vktest` (a staged external binary, built from the Mesa port) is the end-to-end Vulkan check: it `dlopen`s the Venus ICD, enumerates physical devices, and creates a logical device on the real host GPU.

Programs link against **relibc** for full POSIX compatibility, against the lighter `leandros-libc` shim for `no_std` Rust programs, or against **musl** for ported third-party software. Test binaries needing TLS bring-up (`relibc_start_v1`) for real `pthread`/`errno`/`sigaction`/`epoll` support link `librelibc.a` directly.

### Running the suites

`scripts/scmrun.py` drives a QEMU boot over a serial socket and collects results — a persistent serial reader is necessary, because QEMU drops output when nothing is attached and a long test run then looks like a hang.

Run the suites against a **fresh disk image** when the result matters. Images persist across boots, and a suite that has already run against one can leave state that changes the next run's outcome — `O_TRUNC` clears a file's data but not its xattrs, which is exactly the sort of thing that manufactures a phantom architecture-specific failure.

---

## License

GPL-3.0
