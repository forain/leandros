# AArch64 architecture support

The `arch-aarch64` crate covers the exception vectors and fault decoding, the GIC (v2 or v3, detected at boot), the EL1 virtual timer, memory attributes and the EL0 cache-maintenance policy, PSCI for secondary CPUs and power, broadcast TLB maintenance, and the board-specific MMIO (QEMU `virt`, Raspberry Pi 5, QEMU `raspi4b`). The kernel itself is built **soft-float**; why that matters is the most important lesson on this page.

## Overview

| Area | File |
|---|---|
| Init order, device mappings, IRQ hooks | [arch/aarch64/src/lib.rs](../../arch/aarch64/src/lib.rs) |
| Vector table, trap frame, `ret_to_user` | [arch/aarch64/src/exception_asm.s](../../arch/aarch64/src/exception_asm.s) |
| Sync/IRQ handlers, fault → signal | [arch/aarch64/src/exception.rs](../../arch/aarch64/src/exception.rs) |
| GICv2 / GICv3 | [arch/aarch64/src/gic.rs](../../arch/aarch64/src/gic.rs) |
| Generic timer (CNTV), monotonic clock | [arch/aarch64/src/timer.rs](../../arch/aarch64/src/timer.rs) |
| MAIR, SCTLR, FP access | [arch/aarch64/src/mmu.rs](../../arch/aarch64/src/mmu.rs) |
| Page tables, TLB maintenance | [arch/aarch64/src/paging.rs](../../arch/aarch64/src/paging.rs) |
| PSCI, secondary CPUs, MPIDR ↔ index | [arch/aarch64/src/smp.rs](../../arch/aarch64/src/smp.rs) |
| PL011 UART, PL031 RTC, PSCI power-off | `uart.rs`, `rtc.rs`, `power.rs` |
| Kernel target | [targets/aarch64-unknown-kernel.json](../../targets/aarch64-unknown-kernel.json) |

Interrupts in use: PPI **27** (EL1 virtual timer, the tick), SGI **1** (reschedule IPI), SPI **33** (PL011 receive), and level-triggered SPIs that drivers register through `drivers::irq` (QEMU `virt` PCI INTx is SPIs 3–6 after swizzling). PPI 30 (physical timer) is dispatched but never enabled; firmware can leave it armed. There is **no MSI** on AArch64: that needs a GICv3 ITS, which the kernel does not drive.

## How it works

### Init order

`arch_aarch64::init` (after `mm::init_with_map` — [boot.md](boot.md#kernel_main-in-order)):

1. `mmu::enable_identity`: grant FP/SIMD access (`CPACR_EL1.FPEN`), select `SP_EL1`, install MAIR index 2, set the SCTLR EL0 bits (below).
2. Map the PL011 at its HHDM address and switch the UART to it immediately; print `[ARCH] PL011 IBRD/FBRD` (on the Pi this is the only measurement of BCM2712's UART clock) and `[ARCH] MAIR_EL1 before=… after=…`.
3. Map the GIC distributor and CPU interface; on the `virt` build also the whole 64 KiB GICv3 distributor frame, `GICR_MAX_FRAMES` (= `MAX_CPUS`) redistributor frames, and the PL031 RTC. Nothing is touched unless the GIC is detected as v3.
4. Map the framebuffer at `0xFFFF_A000_0000_0000` as **Normal non-cacheable** (MAIR index 2), sized by `BootInfo::framebuffer_bytes()` — one shared source for the mapping, the buddy reservation and the cache sweep, because deriving the size independently once produced a translation fault at `fb_virt + size - 4`.
5. `tlbi vmalle1`, install `VBAR_EL1`, `gic::init`, timer init.
6. Start secondary CPUs (not on `raspi4b`).

### Exception vectors and the EL1 fault path

The vector table is 2 KiB aligned, 16 slots of 128 bytes each branching out of line. An exception from EL0 saves a 288-byte `UserFrame` — `x0`–`x30`, `sp_el0`, `elr_el1`, `spsr_el1` and the task's page-table root — and **no FP/SIMD state** (the context switch saves that; see [a soft-float kernel](#a-soft-float-kernel)). `svc #0` (EC `0x15`) goes to `syscall_dispatch` with `x8` as the number ([syscalls.md](syscalls.md)).

**EL0 synchronous exceptions** ([exception.rs](../../arch/aarch64/src/exception.rs)):

- Data/instruction aborts (EC `0x24`/`0x20`) with a translation or permission fault code go to `sched::handle_page_fault` (demand paging, CoW, file pages). A cache-maintenance instruction's abort sets `ISS.CM` and usually `WnR`; it is serviced as a *read*, as Linux does, so a JIT flushing freshly `mprotect`ed `PROT_READ|PROT_EXEC` code is not refused as a write.
- Anything unresolved is decoded into the signal and `siginfo` Linux would produce: `SIGBUS`/`BUS_ADRERR` past EOF of a file mapping; alignment (`0x21`, PC `0x22`, SP `0x26`) → `SIGBUS`/`BUS_ADRALN`; synchronous external aborts → `SIGBUS`; translation/access-flag/permission faults → `SIGSEGV` (`SEGV_MAPERR`/`SEGV_ACCERR`); unknown, illegal-state and trapped-instruction classes (`0x00`, `0x01`, `0x07`, `0x0E`, `0x18`) → `SIGILL`. If the task has a handler the signal is queued and delivered on the way out; otherwise the whole group dies through `exit_group_signal`. (Fault handlers that killed only the faulting thread used to free shared page tables under running siblings, producing a storm of external-abort-on-table-walk faults.) A gated EL0 frame-pointer backtrace (`EL0_BACKTRACE`) exists for symbolising user faults.

**EL1 synchronous exceptions:** a data abort from the kernel (EC `0x25`) on a **user** address (`FAR[63:48] == 0`) is a kernel or server access to a lazy, CoW or file-backed user page; it is serviced like a user fault and the stub `eret`s back to the faulting kernel instruction. If it cannot be serviced and the PC is inside the `uaccess` copy, the handler rewrites the saved ELR to the copy's landing pad and the syscall returns `EFAULT` ([memory.md](memory.md#how-the-kernel-touches-user-memory)). Otherwise the task is killed with `SIGSEGV`. A fault on a kernel address halts for triage.

Kernel-mode sync exceptions now go straight to `exc_el1_sync`. Until 2026-09-25 they first went through a diagnostic capture stub that used `PAR_EL1` as scratch for `x1`. `PAR_EL1` is not a full 64-bit scratch register on real hardware (RES0/RES1 fields with `F=0`), so under HVF every *recoverable* EL1 fault resumed the kernel with a corrupted `x1`; TCG round-trips the value exactly, which hid it. It surfaced when lazy CoW fork made EL1 write faults on present pages common: a unix-socket `recv` reported 64 bytes and stored them nowhere, and the COSMIC panel's applets respawned in a loop.

**IRQs** go to `handle_irq`, which maintains the per-CPU hard-IRQ depth (`sched::irqctx`) and dispatches through the GIC table, falling back to `drivers::irq::dispatch_line` for lines a driver registered; after 1000 consecutive unclaimed deliveries a line is masked and logged (storm guard).

### GICv2 and GICv3

QEMU `virt` can be either, chosen with `gic-version=`; **QEMU 11.1's HVF refuses to launch a GICv2 machine at all** (`HVF does not support GICv2 emulation`), which silently killed every AArch64 boot on Apple Silicon for nine days with an empty serial log. Since `9063303` (2026-09-14) `gic::init` detects the version and drives either; both launchers pass `gic-version=3`. The Pi builds (a GIC-400) are GICv2-only by `cfg`.

The two versions differ in exactly four places, and [gic.rs](../../arch/aarch64/src/gic.rs) branches only there:

| | GICv2 | GICv3 |
|---|---|---|
| Acknowledge / EOI | memory-mapped `GICC_IAR`/`GICC_EOIR` | `ICC_IAR1_EL1`/`ICC_EOIR1_EL1`, after enabling `ICC_SRE_EL1` (and `ICC_SRE_EL2` before the EL2 drop) |
| SGI/PPI configuration | banked distributor words | per-CPU redistributor, found by matching `GICR_TYPER` affinity to `MPIDR_EL1`, woken through `GICR_WAKER` |
| SPI routing | `GICD_ITARGETSR` CPU mask | `GICD_IROUTER[n]` affinity (with `GICD_CTLR.ARE`) |
| SGIs | `GICD_SGIR` | `ICC_SGI1R_EL1`, targeted by affinity |

**Detection** writes `GICD_CTLR.ARE` with the group enables clear and reads it back — RAZ/WI on v2, sticky on v3 — and prints `[GIC] ID_AA64PFR0.GIC=… PIDR2@0xFE8=… GICD_CTLR after ARE write=… -> GICv3`. Two obvious methods were measured and rejected: `ID_AA64PFR0_EL1.GIC` reads **0 under HVF with `-cpu host`** (QEMU masks it when Apple's in-kernel GIC bypasses the CPU-interface model, so the first build detected v2 on the very machine the port was for), and `GICD_PIDR2` sits at `0xFFE8` on v3, which on a GICv2 board is an unassigned hole above the 4 KiB frame and takes an external abort under TCG.

GICv2 SGIs use each CPU's own interface mask (read from the banked `GICD_ITARGETSR0`) rather than `1 << index`, and SPIs target the BSP's recorded mask; GICv3 addresses CPUs by MPIDR.

### The virtual timer

The tick is the **EL1 virtual timer** (`CNTV`, PPI 27) at 100 Hz. The physical timer (`CNTP`) traps or is restricted in many EL1 environments, and using it produced unknown-class exceptions immediately after the Limine hand-off during the original bring-up ([design/aarch64-boot-investigation.md](../design/aarch64-boot-investigation.md)). Each CPU programs **absolute** compare values on a fixed grid (`EPOCH + k × interval`) and skips — not replays — grid points it missed, reporting how many elapsed, so `sched::ticks()` stays a faithful 10 ms count. Reloading a countdown from "now", as it once did, made the "100 Hz" tick run at 71 Hz idle and 85 Hz under load under HVF, because a `wfi` wake there costs ~4 ms. `CLOCK_MONOTONIC` reads `CNTVCT_EL0` directly against the same epoch ([time.md](time.md)). A per-CPU one-shot deadline is armed on the same timer for poll/futex/hrtimer deadlines.

**Self-healing:** if a CPU has had interrupts enabled and has not ticked for 2 s of counter time, the hypervisor lost the timer edge (the compare value is in the past with nothing re-asserting it). `check_alive`, called from every `irq_window`, programs the next future grid point and logs `[TIMER] cpuN virtual timer silent … re-armed`; the tick that follows accounts every missed grid point. The watchdog's reschedule IPI is what gets a CPU parked in `wfi` with a dead timer to run that check ([scheduler.md](scheduler.md#per-cpu-tick-watchdog)).

### A soft-float kernel

The AArch64 kernel target is `+v8a,-neon,-fp-armv8` with `"abi": "softfloat"` and `"rustc-abi": "softfloat"` — what rustc's own `aarch64-unknown-none-softfloat` does, and what Linux does with `-mgeneral-regs-only`. The x86-64 kernel was already soft-float. The only kernel code that touches vector registers is the context switch, which saves and restores userspace `q0`–`q31` inside an explicit `.arch armv8-a+fp+simd` … `.arch armv8-a` bracket ([sched/src/context.rs](../../sched/src/context.rs)).

*Why this is load-bearing (2026-08-05):* the kernel target used to say `+neon,+fp-armv8`, while the EL0 trap frame saves no vector state. LLVM lowered the file-backed demand-paging path's bulk copy (a 64 KiB fault-around read plus `copy_nonoverlapping`) through `q` registers and returned to EL0 with the faulting thread's `q0` overwritten by a kernel HHDM pointer — on **exactly 1 in 16** file-backed page touches (960/15360), one per gathered read. Because the first execution of a function is itself a demand fault, the *first* SipHash in a process computed a different value from every later one; a `HashMap` stored an entry under an unreachable hash, so `iter()` showed `("path", "/run/user/0/bus")` while `get("path")` returned `None`, and zbus rejected a perfectly valid D-Bus address — only on AArch64, only in a 52 MB binary with many cold pages. After the fix: 0/15360, and every regression suite byte-identical to the pre-change baseline. The SVC, timer-IRQ and anonymous-fault paths had all measured clean; **a syscall-only test does not prove the trap paths are safe — test the fault path explicitly.**

### Memory attributes (MAIR)

`mmu::enable_identity` read-modify-writes `MAIR_EL1`, installing **Normal Inner/Outer Non-cacheable (`0x44`) at index 2** and leaving the loader's attributes 0 and 1 untouched, because the loader's live mappings use them. Limine 11.4.1 writes `MAIR_EL1 = 0xFF | (dev << 8)` and the direct-boot path `0x04FF`, so attributes 2–7 are zero in both — and a zero byte is **Device-nGnRnE**. Index 3, the `ATTR_NOCACHE` that `PageFlags::NOCACHE` selects, therefore *is* Device-nGnRnE on every boot path: right for MMIO, wrong for anything userspace `memcpy`s through (Device memory faults on the first unaligned access), and painfully slow for the framebuffer, where every 4-byte store becomes an individual non-posted transaction. `PageFlags::WRITECOMBINE` selects index 2: still uncached and coherent with the host and scanout, but gathering and reordering are allowed. Normal memory is buffered, so the framebuffer flush path issues a `dsb` before ringing the virtio-gpu doorbell. `smp_init` snapshots the finished MAIR for every AP. See [memory.md](memory.md#kernel-mappings-device-memory-and-the-kva-window).

### EL0 cache maintenance (SCTLR)

`SCTLR_EL1` gets **UCT** (bit 15: EL0 may read `CTR_EL0`), **UCI** (bit 26: EL0 may run `DC CVAU/CVAC/CIVAC/CVAP` and `IC IVAU`) and **DZE** (bit 14: EL0 may run `DC ZVA`), as Linux's `INIT_SCTLR_EL1_MMU_ON` does. Every JIT reads `CTR_EL0` and cleans the caches between writing code and running it (libgcc's `__clear_cache`, LLVM, SpiderMonkey, V8); without UCT/UCI those instructions trap with EC `0x18` and the process dies. They act by virtual address with the caller's own permissions, so they reach nothing the process could not already read or write. The BSP sets them; APs copy the BSP's SCTLR.

The kernel does its own cache maintenance where it writes code for userspace: the ELF loader cleans each executable page by VA through the HHDM and invalidates the instruction cache once; every page entering the page cache is cleaned to the point of coherency, and a cache hit in an executable mapping does `ic ialluis`; the eager file-mapping path cleans the I-cache for exec mappings.

### TLB maintenance

Per page: `tlbi vaae1is` + `dsb ish`, broadcast to the inner-shareable domain; above 16 pages, `tlbi vmalle1is`. No IPIs. An in-place read-only → writable upgrade adds a local `tlbi vaale1`. See [memory.md](memory.md#tlb-shootdown).

### PSCI and secondary CPUs

Secondary CPUs are enumerated from the ACPI MADT GICC entries, else the device tree's `/cpus/cpu@*` (`device_type = "cpu"`, status okay), else the legacy guess of Aff0 1..7, and started **one at a time** with PSCI `CPU_ON` (`0xC400_0003`). PSCI delivers an AP with the MMU off at the stub's physical address, so the stub drops EL2→EL1 if needed, programs MAIR/TCR/TTBR0/TTBR1/SCTLR from the BSP's values (`AP_BOOT_PARAMS`, cleaned to the point of coherency first — QEMU does not care, real cores do), enables the MMU through a dedicated identity-mapped TTBR0 root, and jumps to its virtual continuation. Each AP gets a 64 KiB boot stack, initialises its banked GIC state and local timer, and parks until the scheduler opens. A failed `CPU_ON` (including `ALREADY_ON`, since that CPU never ran our stub) gives its index back.

`arch_cpu_id()` is `mrs mpidr_el1` plus one byte load from a 1024-entry table keyed by `Aff0[3:0] | Aff1[3:0]<<4 | Aff2[1:0]<<8`, zero-initialised so everything maps to 0 before `smp_init`, when only the BSP runs. The old code used `MPIDR.Aff0` directly as the index — wrong on the Pi 5, whose Cortex-A76 puts the core number in Aff1 (every core was index 0), and on QEMU `virt` with more than 16 vCPUs (the 17th has MPIDR `0x100`). The old hard-coded 1..7 list also made `CPU_ON` fail with `INVALID_PARAMS` on the Pi, so it ran single-core by accident ([scheduler.md](scheduler.md#dense-cpu-indices-decided-at-bring-up)).

The **conduit** is HVC unless the kernel was entered at EL2 (QEMU then registers PSCI on SMC) or is an `rpi5` build (TF-A at EL3, reached by SMC; HVC would fault with no hypervisor). `SYSTEM_OFF` (`0x8400_0008`) and `SYSTEM_RESET` (`0x8400_0009`) implement `reboot(2)`. On `raspi4b` SMP is skipped: that board releases all cores at reset and its SMC dispatch hangs on `CPU_ON` for an already-running core.

## Design decisions

### Detect the GIC version by behaviour, not by ID register

Covered above: both ID-register methods were measured to give the wrong answer or fault on one of the two configurations the code must run on. The `ARE` write-and-read-back is defined on both and harmless on both. The port was small because the kernel consumes only four interrupt sources and allocates no MSI, so no ITS was needed.

### Normal-NC for anything bulk, Device for registers

`WRITECOMBINE` and `NOCACHE` stay separate flags so a GPU blob can be uncached without turning a sound card's registers into speculatively readable Normal memory, and so the framebuffer gets combined bursts instead of millions of non-posted single stores per scroll.

## Limitations and open work

- **No PAN** (`PSTATE.PAN` is never set), and executable user pages clear PXN as well as UXN, so the kernel could execute user code ([memory.md](memory.md#limitations-and-open-work)).
- **No MSI/MSI-X**: `request_msi` and `MsixTable::enable` return `Unsupported` until a GICv3 ITS driver exists.
- `AT_HWCAP` is 0, so userspace selects baseline (non-crypto, non-atomics-extension) code paths.
- The direct-boot park loop still elects on `MPIDR.Aff0` (the `raspi4b` path).
- AArch64 direct boot under HVF hangs (an upstream QEMU class); UEFI/Limine is the default under HVF ([boot.md](boot.md#limitations-and-open-work)).
- HVF constraint for drivers: if LLVM compiles an MMIO `read_volatile` into a writeback addressing form (`ldrb w8, [x19, #2]!`), the access traps with `ISV=0`, which HVF cannot emulate, and QEMU aborts with `Assertion isv failed`. virtio-input's config accessors use plain `ldrb`/`strb` asm for this reason; other drivers' `read_volatile` MMIO has the same latent exposure whenever codegen picks a writeback form ([hardware/virtual-machines.md](../hardware/virtual-machines.md)).
- Pi 5 SMP and the cache-cleaned `AP_BOOT_PARAMS` are untested on the board since the dense-index rewrite; watch the first `[SMP]` line ([hardware/raspberry-pi-5.md](../hardware/raspberry-pi-5.md)).

## Code and tests

Code: [arch/aarch64/src](../../arch/aarch64/src), the entry in [kernel/src/entry_aarch64.s](../../kernel/src/entry_aarch64.s), and the context switch in [sched/src/context.rs](../../sched/src/context.rs).

Tests: the whole `regress` suite on HVF (GICv3) and on TCG with `LEANDROS_QEMU_EXTRA="-machine virt,gic-version=2"` (the GICv2 control; QEMU merges duplicate `-machine` options, last wins); `LEANDROS_SMP=20` exercises MPIDRs above `0xF`, and `-machine acpi=off` the device-tree enumeration path; `memtest`, `forktest` and `uptrtest` exercise EL0 and EL1 fault paths; `ktest=msi` with QEMU's `edu` device covers the INTx path (GIC INTID 37). The FP/SIMD clobber had a dedicated reproducer that loads `q0`–`q31`, touches a cold file-backed page and diffs the registers; any change to the trap path should repeat that check. Boot lines to look for: `[ARCH] MAIR_EL1 before=… after=…`, `[GIC] … -> GICv3`, `[SMP] …`.

## See also

- [arch-x86_64.md](arch-x86_64.md) — the other architecture
- [boot.md](boot.md) — `entry_aarch64.s`, EL drops, the early page tables
- [scheduler.md](scheduler.md) — context switch and FP/SIMD policy
- [hardware/virtual-machines.md](../hardware/virtual-machines.md) — QEMU machines, HVF, GICv3
- [hardware/raspberry-pi-5.md](../hardware/raspberry-pi-5.md) — the GIC-400 board
