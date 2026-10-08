# Design principles

The rules below cut across every subsystem. Each one was adopted because its absence cost something measurable; the reasons are given with each, and the subsystem pages carry the detail.

## Architecture

### A microkernel core behind a Linux personality

The core does scheduling, IPC and memory management; file systems, graphics, audio, input, terminals and networking are servers reached through ports ([overview](README.md#architecture-at-a-glance), [IPC](../kernel/ipc.md), [servers](../servers/README.md)). In front of that sits a dispatcher that implements the **real Linux syscall numbers** for each architecture ([syscalls](../kernel/syscalls.md)).

*Why:* the software worth running — Mesa, a Wayland compositor, COSMIC, Firefox — is written against Linux. A private ABI would need a patched libc and patched programs forever; a Linux personality needs neither. The original ten-call IPC ABI survives underneath for the system's own components.

### Match Linux exactly, including its errors

When LeandrOS and Linux disagree, LeandrOS is wrong — including about error codes, error *ordering* and corner cases that look like bugs. Examples of the rule in practice:

- Creating an entry checks *search* permission, then `EEXIST`, then *write* permission, because `mkdir` of an existing name must report `EEXIST`, not `EACCES` ([security](../servers/security.md)).
- A timed `FUTEX_WAIT` interrupted by a handled signal returns `EINTR` and is not restarted, because Linux never restarts it — a "race" filed against this behaviour was refuted ([threads and futexes](../kernel/threads-and-futex.md)).
- `epoll_ctl` returns `EEXIST`/`ENOENT`/`EPERM` exactly as Linux does, and epoll interest is keyed by the open file description so it survives `dup`, `fork` and `SCM_RIGHTS` ([event loop](../kernel/event-loop.md)).
- Running out of file descriptors in a GPU submit kills the client's context, as on Linux; the defence is fd headroom (512 per process), not a non-Linux error path.

The strongest form of the rule is a conformance test written against a real Linux kernel first: `privtest` and `nettest` are compiled unchanged for a Linux host and for LeandrOS, so every expectation in them has been checked against Linux ([testing](../development/testing.md)).

### Zero COSMIC patches

The desktop is built from unmodified upstream sources. When COSMIC misbehaves on LeandrOS, the fix goes into the kernel, the servers or a port's *environment* (a launcher script, a staged config file), never into COSMIC's code. The rule has produced most of the kernel's Linux fidelity: an `env_rx` timeout patch to `cosmic-session` was retired once the underlying futex and clone bugs were fixed (2026-09-16), leaving zero patches.

There is exactly one approved exception: `ports/cosmic-comp/0001-toplevel-info-weak-window.patch` (2026-09-27), a port of an upstream community fix for a leak that exists on Linux too (one GL texture kept alive per closed window). It is scoped to that leak. A known `cosmic-term` use-after-free on exit is tracked as an upstream bug and deliberately left unpatched. See [desktop](../desktop/README.md).

### GPU rendering only, never software

COSMIC renders on the host GPU — Zink over Venus, or virgl — or does not start. The guest's `/bin/gpu-env` verifies a hardware `GL_RENDERER` before any compositor runs; with none, init prints a `NO GPU RENDERER` banner and only the text logins come up. Software rendering is an opt-in for debugging (`touch /etc/leandros/allow-software-render`).

*Why:* a softpipe desktop was slow enough to hide real bugs behind rendering cost and invite the wrong fixes — a greeter keystroke-lag fix that disabled a blur effect under softpipe was reverted (2026-09-25) because it patched COSMIC to dodge a cost that does not exist on a GPU. Performance is measured on the GPU path; softpipe numbers are informational only. See [graphics](../graphics/README.md) and [Zink and virgl](../graphics/zink-and-virgl.md).

### A Rust repository

The kernel, the servers, the drivers and every in-tree program are Rust. Throwaway probes written in C stay outside the repository once they have delivered their finding; a capability worth keeping is rewritten as a `no_std` Rust crate under `userland/` and joins the regression suites. The build system is not taught a C path to make an import convenient. C appears only where it *is* the interface: ABI shims for third-party C libraries (`ports/input-stack/shims`, `ports/openrm/glue`, port glue for Mesa, PipeWire and Firefox), vendored third-party files, and the two Linux-conformance tests above, which are C precisely so the same source runs on a Linux host. The full list of tracked C files is in [userland](../userland/README.md); see also [conventions](../development/conventions.md).

## Correctness rules in the kernel

These are summarised here and specified in [kernel invariants](../kernel/invariants.md).

| Rule | Why | Detail |
|---|---|---|
| **No `unsafe` globals beyond the boot parsers.** Static mutable state is wrapped in `spin::Mutex`; the only bare `static mut` blocks are in the boot parsers (single-threaded, before any AP starts) and the arch assembly stubs. | Shared state on an SMP kernel needs a lock or an atomic; a bare global is a race by construction. | [invariants](../kernel/invariants.md) |
| **Never touch user memory under a scheduler spinlock.** All cross-boundary copies go through `AddressSpace::read_user_buf`/`write_user_buf` outside the lock. | A demand-paging fault taken while holding `RUN_QUEUE` with interrupts off re-enters the scheduler lock and freezes every vCPU with no panic and no output. | [invariants](../kernel/invariants.md), [memory](../kernel/memory.md) |
| **W^X.** `sys_map_mem` and `mmap` reject writable+executable mappings. | Kept even for Firefox: its JIT is configured W^X rather than the kernel relaxing the policy. | [memory](../kernel/memory.md) |
| **Checked user-copy discipline** — every cross-boundary copy bounds-checked against the user range, fault-tolerant copies under locks, `CR0.WP` on every CPU. Hardware SMAP/SMEP/PAN is *not* enabled yet (open work). | A bad user pointer must fail as an errno, never as a kernel fault or a silent write through a read-only page. | [memory](../kernel/memory.md#smap-is-a-discipline-here-not-a-hardware-feature), [x86-64](../kernel/arch-x86_64.md) |
| **Checked arithmetic at memory boundaries** — VMA ends, slab orders, DTB offsets use `checked_add`. | Wrapping arithmetic on attacker- or firmware-supplied values is a silent memory-safety hole. | [memory](../kernel/memory.md), [boot](../kernel/boot.md) |
| **Demand paging** — reserved address space is backed on first touch; fork is copy-on-write and lazy. | `cosmic-comp` used to copy ~210 MiB eagerly on every fork. | [memory](../kernel/memory.md) |
| **Per-CPU syscall stacks (x86-64)** — `swapgs` + `%gs:0/8` load the kernel stack and save the user RSP. | No shared state on the syscall entry path. | [x86-64](../kernel/arch-x86_64.md) |
| **Auxv service discovery** — server port ids are stamped into each task's auxiliary vector. | O(1) lookup, no name-service daemon to start first. | [IPC](../kernel/ipc.md) |
| **Asynchronous audio pipeline** — the PipeWire server's spool buffer decouples client timing from the VirtIO hardware ring; the server drains it at interrupt cadence. | Clients never block on a full ring, which prevents audio starvation and client freezes. | [audio](../audio.md) |
| **Non-blocking DRM mmap** — userspace maps the framebuffer directly (`DRM_IOCTL_VIRTGPU_MAP`, backed by `map_kernel_device`). | Bypasses the VFS write path for display updates, giving the throughput full-frame rendering needs. | [DRM/KMS](../graphics/drm-kms.md) |
| **Exit-log side table** — reaped tasks leave their exit code in a 256-slot `EXIT_LOG` keyed by PID; `wait_pid` falls back to it. | Removes the race between the reaper and the waiter. | [processes](../kernel/processes.md) |
| **One resolver for every path syscall** — path resolution, symlinks, `AT_*` dirfds and chroot confinement live in a single place. | `renameat` honouring its dirfds was the difference between a clean desktop session log and a storm of `ENOENT`s from atomic state writes. | [syscalls](../kernel/syscalls.md) |
| **Round up, never truncate, when converting time; program timers as absolute deadlines.** | `nanosleep` truncating every sub-tick sleep to zero passed every test for months, until Mesa's watchdog fired 200x early and `abort()`ed. `FUTEX_WAIT_BITSET` read as relative made every Rust timed wait last uptime + timeout. A tick counter is not a clock: the tick-derived clock ran 5–36 % slow on every accelerator until 2026-09-16. | [time](../kernel/time.md) |
| **Kernel stack frames are budgeted at build time.** `build-all.sh` emits `.stack_sizes` and refuses a kernel with any frame over 48 KiB. | Kernel stacks (128 KiB) have no guard page; a 156 KB frame in `f2fs::mount` silently overwrote neighbouring allocations and spent a day looking like a host-dependent boot failure. | [building](../getting-started/building.md), [scheduler](../kernel/scheduler.md) |

## Diagnosability

### An errno must name its own cause

`ENOMEM` from this kernel used to mean about a dozen unrelated things, only two of them about RAM, and userspace saw the number and nothing else. Two separate investigations went hunting for a memory leak while the guest reported over a gigabyte free; the real cause was a 64-entry IPC port table. Every `ENOMEM` in the memory, address-space and exec paths now routes through `enomem_site()` ([`kernel/src/syscall.rs`](../../kernel/src/syscall.rs)) behind a gated, budgeted trace, so the errno cannot drift away from its explanation. `sys_brk` is included even though `brk(2)` has no errno — musl detects a heap that cannot grow by comparing the returned break and turns it into a `NULL` from `malloc`, which used to be invisible. `sys_mincore` is deliberately excluded: its `ENOMEM` means "this range is not mapped" and is a normal answer to a pointer probe.

### Debug output is gated and off by default

Render and audio hot paths, task lifecycle, `[MMAP]`/`[CPIO]` tracing, EL0 fault backtraces and the unix-socket exchange trace are all behind constants that default to off. Unconditional serial writes on a hot path do not merely slow the system down; they change its timing enough to hide the race being investigated. When gating a helper, check for local copies of it — one server had a private duplicate of the print helper, and gating only the shared one left unlabelled hex digits streaming per frame. Kernel diagnostic lines are printed whole through a per-CPU outbox so they cannot splice into user output ([debugging](../development/debugging.md)).

### Measure at the destination, and prove the instrument first

Four separate defects in this tree were once recorded with the wrong cause and evidence beside them, and in three of the four *the instrument or the harness was the defect*: a serial reader that wedged the timer tick and then reported input loss, a completion marker matched from the tty's own echo, a probe that reported "no matches" because the guest has no `grep`. What separated the recorded cause from the truth was never more reasoning — it was instrumenting at the destination, a control that varied exactly one thing, and a test shown to fail before it was trusted. See [methodology](../development/methodology.md).

## Process

- **Both architectures, always.** Every change is built and tested on x86-64 and AArch64; cross-platform parity is an invariant, not a goal. Name results by *accelerator* (HVF, KVM, TCG), not by architecture — a slow accelerator exposes races a fast one hides ([methodology](../development/methodology.md)).
- **Green QEMU is evidence about logic, not hardware.** Whole classes of AArch64 bugs (cache-off exclusives, flat-binary entry, board memory maps, peripheral clocks) are invisible in emulation by construction ([methodology](../development/methodology.md), [Raspberry Pi 5](../hardware/raspberry-pi-5.md)).
- **Release builds only**, and the Limine boot protocol revision never drops below **6** ([conventions](../development/conventions.md), [boot](../kernel/boot.md)).
- **Build outputs come from tracked source.** Anything staged into the image is regenerated from the repository on every build, and the image builder refuses a staged payload older than its source — an activation feature once sat in the repository and in no image for two days while a boot test called it green ([building](../getting-started/building.md)).

## See also

- [Kernel invariants](../kernel/invariants.md) — the hard rules, precisely.
- [Development methodology](../development/methodology.md) — the refuted premises behind several of these rules.
- [Conventions](../development/conventions.md).
