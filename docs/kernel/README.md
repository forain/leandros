# The kernel

The LeandrOS kernel is a `no_std` Rust kernel for x86-64 and AArch64 built from a small microkernel core — physical and virtual memory, an SMP scheduler, and port-based IPC — with a Linux-compatible system-call personality in front of it. Filesystems, graphics, audio, input, terminals and networking are *servers*: Rust crates reached through IPC ports, linked into the kernel image and run synchronously in the calling task's kernel context. This section documents the core; the servers have [their own section](../servers/README.md).

## Overview

```
              user space (EL0 / ring 3)
   musl / relibc programs ── Linux syscall numbers ── svc #0 / syscall
 ─────────────────────────────────────────────────────────────────────────
              kernel (EL1 / ring 0), IRQs masked for the whole syscall
   kernel/src/syscall.rs  dispatcher: Linux personality + private 510..590
        │                      │                         │
   sched (tasks, EEVDF,   ipc (ports, messages,     mm (buddy, slab, VMM,
   SMP, signals, futex,   sys_call reply ports)     demand paging, CoW,
   watchdog, kthreads)          │                   page cache, KVA)
                                ▼
   servers/* (vfs, f2fs, net, tty, drm, evdev, pipewire, xattr):
   port handlers called in the caller's context
        │
   drivers/* (virtio, PCI, xHCI/USB, SDHCI, framebuffer, …)
        │
   arch/x86_64 | arch/aarch64  (paging, interrupts, timers, SMP bring-up)
   boot        (Limine / DTB / multiboot2 / ACPI parsing)
```

The original architecture diagram drew the servers as a separate EL0 layer. That is the *design shape* — every server is reached only through a port and owns its state — but not the execution model: `vfs_server`, `f2fs_server`, `net_server`, `tty_server`, `drm_server`, `evdev_server`, `pipewire_server` and `xattr` are dependencies of the `kernel` crate ([kernel/Cargo.toml](../../kernel/Cargo.toml)), started in `init_task_main` under the heading "In-Kernel Servers" ([kernel/src/init.rs](../../kernel/src/init.rs)), and a message sent to a port with a registered handler runs that handler immediately, on the sender's kernel stack ([ipc.md](ipc.md#direct-handlers-servers-run-in-the-callers-context)). The reasons and consequences are in [servers/README.md](../servers/README.md#synchronous-in-caller-dispatch-rather-than-server-threads).

## Crate map

| Crate | Path | Role |
|---|---|---|
| `kernel` | [kernel/src](../../kernel/src) | Entry points (`entry_x86_64.s`, `entry_aarch64.s`), `kernel_main`, kernel init task, syscall dispatcher (`syscall.rs`, ~11.8k lines), console, persistent log (`klog.rs`), power-off/reboot (`power.rs`), firmware loading (`firmware.rs`), boot self-tests (`ktest.rs`) |
| `mm` | [mm/src](../../mm/src) | Buddy allocator, slab, per-process `AddressSpace`/VMAs, demand paging, copy-on-write fork, page refcounts, shared page cache, kernel VA window (`ioremap`/`vmap`), IRQ-safe atomic pool |
| `sched` | [sched/src](../../sched/src) | Tasks and the run queue (EEVDF), SMP dispatch, context switch, signals, futexes, clone/fork, credentials, CSPRNG, watchdog, lock profiling, user-copy fixups, kernel threads, sleeping locks, work queues, hrtimers |
| `ipc` | [ipc/src](../../ipc/src) | Ports, the `Message` format, `Channel` |
| `boot` | [boot/src](../../boot/src) | Limine response parsing, flattened device tree, multiboot2, ACPI (MADT, MCFG, FADT) |
| `elf` | [elf/src](../../elf/src) | ELF64 loader (`load`, `load_lazy`), ET_EXEC and ET_DYN with a load bias, PT_INTERP |
| `arch-x86_64` | [arch/x86_64/src](../../arch/x86_64/src) | GDT/TSS/IDT, LAPIC/x2APIC/IOAPIC, PAT, SYSCALL entry, TSC calibration, SMP trampoline, TLB shootdown, PS/2 keyboard |
| `arch-aarch64` | [arch/aarch64/src](../../arch/aarch64/src) | Exception vectors, GICv2/GICv3, generic timer, MMU attributes, PSCI SMP, PL011 UART, PL031 RTC |
| `capability` | [capability/src](../../capability/src) | A per-process capability-token table. **Not linked into the kernel**: it is a workspace member with no dependents. File-backed VMAs use their own `file_cap` tokens in `mm::vmm` instead. |

Workspace-wide layout and the sibling repositories are in [overview/workspace-layout.md](../overview/workspace-layout.md).

## Pages in this section

Core (this writer's pages):

- [Boot](boot.md) — Limine (base revision 6, never lower), direct DTB boot, EL2→EL1, HHDM, the order of `kernel_main`, and the hand-off to userspace `init`.
- [Memory management](memory.md) — buddy and slab, `AddressSpace`/VMAs, demand paging, lazy copy-on-write fork, the shared page cache, tmpfs/memfd VMOs, W^X, user-copy accessors, and the `ENOMEM` sites.
- [Scheduler and SMP](scheduler.md) — the global EEVDF run queue, dispatch, preemption, FP/SIMD policy, CPU bring-up and dense CPU indices, the tick watchdog, idle, affinity, the `Stopped` state, kernel threads and sleeping locks.
- [IPC](ipc.md) — ports, messages, `sys_call` reply ports, direct handlers, the `LIVE_BUCKETS` story, and auxv service discovery.
- [System calls](syscalls.md) — the original native IPC ABI, the Linux personality, per-architecture numbers, the single path resolver, and errno conventions.
- [x86-64](arch-x86_64.md) — GDT/TSS/IDT, APIC and x2APIC, per-CPU SYSCALL stacks, PAT on every CPU, TLB shootdowns, MSI/MSI-X vectors.
- [AArch64](arch-aarch64.md) — exception vectors, GICv2/GICv3, the virtual timer, the softfloat kernel, MAIR and SCTLR policy, PSCI.
- [Kernel invariants](invariants.md) — the hard rules and the bugs that taught each one.

Process model (separate pages):

- [Processes](processes.md) — thread groups, `fork`/`execve`, the ELF loader as `execve` uses it, exit and reaping, credentials.
- [Threads and futexes](threads-and-futex.md) — `clone(CLONE_VM)`, futex semantics and keying.
- [Signals](signals.md) — delivery, frames, `sigaltstack`, fault `siginfo`, job-control stops.
- [Time and timers](time.md) — the monotonic clock, the tick grid, POSIX timers, `timerfd`, `nanosleep`.
- [The event loop](event-loop.md) — `poll`/`select`/`epoll`, the poll wait channel, readiness edges.

## Execution model in one paragraph

Every CPU runs `scheduler_run_loop` on its own scheduler context and picks from **one shared run queue** ([scheduler.md](scheduler.md)). A task enters the kernel through `svc #0` or `syscall` and stays there **with interrupts masked** until it returns to user mode: kernel code is not preemptible except at explicit points (`yield_now`, a blocking primitive's commit, or an `irq_window()` that briefly unmasks to take a pending tick). Every kernel `spin::Mutex` is therefore an IRQ-off spinlock, which is why a deadlock is silent and why the per-CPU tick watchdog and `lockwatch` exist ([scheduler.md](scheduler.md#per-cpu-tick-watchdog)). Server handlers, page-fault service and filesystem reads all run on the calling task's 128 KiB kernel stack, so its depth is policed at build time and checked on every syscall return ([invariants.md](invariants.md#kernel-stacks-are-128-kib-unguarded-and-policed)).

## Other kernel services

These have no page of their own:

- **CSPRNG** — [sched/src/random.rs](../../sched/src/random.rs): ChaCha20 with fast key erasure behind `getrandom(2)`, `/dev/urandom` and `/dev/random`. It is seeded in `kernel_main` before any user process exists, from virtio-rng, the device tree's `/chosen/rng-seed`, the CPU generator and timing jitter.
- **Persistent kernel log** — [kernel/src/klog.rs](../../kernel/src/klog.rs): everything that reaches the console is copied into a lock-free 2 MiB RAM ring and appended to a raw log partition on the boot disk (4 slots, `LEANLOG1` header with CRCs), flushed every 2 s, on `sync`, on power-off and on panic. It exists for real-hardware boots where the serial port is not reachable; `scripts/read-usb-log.sh` reads it back. See [hardware/pc.md](../hardware/pc.md).
- **Boot self-tests** — [kernel/src/ktest.rs](../../kernel/src/ktest.rs): `ktest=all` (or a list) on the kernel command line runs the kernel-facility tests in a `[ktest]` thread and prints `[KTEST] summary`. See [scheduler.md](scheduler.md#kernel-threads-sleeping-locks-and-deferred-work).
- **Power** — [kernel/src/power.rs](../../kernel/src/power.rs): `reboot(2)` through the ACPI FADT on x86-64 and PSCI `SYSTEM_OFF`/`SYSTEM_RESET` on AArch64.

The README's diagram once listed an interactive "kernel shell"; no such thing exists in the tree any more — the first user program is `init`, and the serial getty is a userspace process ([getting-started/logging-in.md](../getting-started/logging-in.md)).

## See also

- [overview/README.md](../overview/README.md) — what LeandrOS is and where it stands
- [overview/design-principles.md](../overview/design-principles.md) — the cross-cutting rules
- [servers/README.md](../servers/README.md) — the server layer
- [development/debugging.md](../development/debugging.md) — `[WDOG]`, Ctrl-T, serial capture
- [design/README.md](../design/README.md) — historical design records (K2 epoll, K3 ELF, K4 DRM)
