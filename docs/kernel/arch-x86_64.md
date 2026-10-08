# x86-64 architecture support

The `arch-x86_64` crate is everything the kernel needs from an x86-64 CPU: per-CPU GDT/TSS and the IDT, the local APIC (xAPIC or x2APIC) and IOAPIC, the SYSCALL entry with per-CPU kernel stacks, the PAT set up identically on every CPU, TSC calibration and the APIC timer, SMP start-up with dense CPU indices, targeted TLB shootdowns, and the dynamic interrupt vectors behind MSI and MSI-X.

## Overview

| Area | File |
|---|---|
| Init order | [arch/x86_64/src/lib.rs](../../arch/x86_64/src/lib.rs) |
| GDT, per-CPU TSS, IST | [arch/x86_64/src/gdt.rs](../../arch/x86_64/src/gdt.rs) |
| IDT, exception and IRQ stubs, fault → signal | [arch/x86_64/src/idt.rs](../../arch/x86_64/src/idt.rs) |
| SYSCALL entry, per-CPU syscall data | [arch/x86_64/src/syscall.rs](../../arch/x86_64/src/syscall.rs) |
| Local APIC, x2APIC, IPIs | [arch/x86_64/src/apic.rs](../../arch/x86_64/src/apic.rs) |
| CPU enumeration (MADT), APIC ID ↔ index | [arch/x86_64/src/cpus.rs](../../arch/x86_64/src/cpus.rs) |
| AP trampoline and start-up | [arch/x86_64/src/smp.rs](../../arch/x86_64/src/smp.rs) |
| Page tables, PAT, TLB shootdown | [arch/x86_64/src/paging.rs](../../arch/x86_64/src/paging.rs) |
| APIC timer, TSC frequency, monotonic clock | [arch/x86_64/src/timer.rs](../../arch/x86_64/src/timer.rs) |
| IOAPIC, 8259 masking, PS/2 keyboard, RTC, ACPI power | `ioapic.rs`, `pic.rs`, `keyboard.rs`, `rtc.rs`, `power.rs` |

Fixed vectors: 0–31 exceptions, **32** the timer, **33** the PS/2 keyboard (IOAPIC IRQ 1, registered as a fixed vector), **0x40** the reschedule IPI, **0xFD** the TLB-shootdown IPI, **0xFF** spurious. Every other vector 0x21–0xEF is a dynamic device vector.

## How it works

### Init order

`arch_x86_64::init` (called from `kernel_main` after `mm::init_with_map` — [boot.md](boot.md#kernel_main-in-order)):

1. `paging::init_pat_bsp()` — first, before anything is mapped ([PAT](#pat-identical-on-every-cpu)).
2. `gdt::init`, `idt::init`.
3. `enable_sse` — clear `CR0.EM`/`CR0.TS`, **set `CR0.WP`**, set `CR4.OSFXSR`, `CR4.OSXMMEXCPT`, `CR4.FSGSBASE`.
4. Map the LAPIC page (Limine revision ≥ 1 does not map MMIO in the HHDM) and the framebuffer write-combining.
5. `cpus::discover` (MADT) **before** `apic::init`, because the APIC mode depends on the IDs found and the BSP's dense index must exist before `arch_cpu_id` is seeded.
6. `apic::init`, IOAPIC (IRQ 1 → the BSP's APIC ID, vector 33), `timer::init`, `syscall::init`.
7. Record the boot CR3 as the canonical kernel root (every CPU reloads it after running a task, so a freed task root is never left loaded).
8. `smp::smp_init` — last, because the APs inherit everything above.

### GDT, TSS and the IDT

Each CPU has its own GDT (the TSS descriptor embeds that CPU's TSS address) and its own TSS: a single shared TSS would make CPUs overwrite each other's `RSP0` and take user-mode interrupts on the wrong CPU's stack. The selector layout is fixed by SYSCALL/SYSRET: `0x08` kernel code, `0x10` kernel data, `0x18` user data, `0x20` user code, `0x28` the 16-byte TSS descriptor; `STAR` = `0x08 << 32 | 0x10 << 48`. **IST1** is a dedicated 4 KiB per-CPU double-fault stack, so a fault caused by an exhausted kernel stack does not immediately triple-fault.

User-mode CPU exceptions are decoded per vector into the signal Linux would send, with a filled-in `siginfo` (`#DE` → `SIGFPE`/`FPE_INTDIV`, `#PF` → `SIGSEGV` with `SEGV_MAPERR`/`SEGV_ACCERR` or `SIGBUS`/`BUS_ADRERR` past EOF, `#UD` → `SIGILL`, …), delivered through `exit_group_signal` when unhandled so the whole thread group dies and `waitpid` reports `WIFSIGNALED` ([signals.md](signals.md)). Fault handlers that used to call single-thread `exit(1)` freed shared page tables under running sibling threads; every fatal user fault now ends the group. A kernel-mode `#PF` on a user address is first serviced as a demand fault; if it cannot be, the `uaccess` fixup table is consulted ([memory.md](memory.md#how-the-kernel-touches-user-memory)); otherwise the task is killed with `SIGSEGV` instead of halting the CPU (2026-09-27).

### SYSCALL entry and per-CPU stacks

Each CPU has a `PerCpuSyscall { kernel_stack_top, user_rsp_save }` (offsets `gs:0` and `gs:8`), and `IA32_KERNEL_GS_BASE` points at that CPU's struct. `syscall_entry` executes `swapgs`, saves the user RSP to `%gs:8`, loads the kernel stack from `%gs:0`, builds a `UserFrame`, and calls `syscall_dispatch` with the System V ABI; the return path restores and `swapgs`es back. No shared state is touched between entry and the stack switch — the original implementation used one global user-RSP slot and one global stack, which is SMP-unsafe by construction. `set_syscall_stack` updates the current CPU's slot on every context switch.

`FMASK` clears `IF` (the kernel runs with interrupts masked for the whole syscall), `DF` (LLVM lowers struct copies to `rep movsq`, which with `DF=1` copies downward — a hand-rolled `std; syscall` would otherwise corrupt the kernel), `TF`, `NT` and `AC`. `EFER.SCE` and `EFER.NXE` are set per CPU here, since `STAR`/`LSTAR`/`FMASK`/`EFER` are per-CPU MSRs.

The entry assembly uses deliberate shortcuts — for instance it trusts the live `rax` for the return value instead of reloading the saved slot — so **any edit to it must be verified by a real interactive round trip**, not just a successful boot: inserting a call that clobbered `rax` once looked fine through boot, `exec` and several `write`s and then hung on the first interactive `read()`. The workflow is in [development/debugging.md](../development/debugging.md).

### APIC and x2APIC

`apic::init` masks the 8259 PICs, enables the LAPIC through `IA32_APIC_BASE` and the spurious-vector register, and seeds `IA32_TSC_AUX` with this CPU's dense index so `arch_cpu_id()` is one `RDTSCP` ([scheduler.md](scheduler.md#kernel-overhead-is-measured-at-the-instruction-not-guessed)). Register access switches to the x2APIC MSRs (`0x800 + off >> 4`) when the firmware handed over in x2APIC mode or an enabled CPU has an ID above 254 and CPUID advertises x2APIC; the BSP decides and every AP follows. In x2APIC mode the ICR is the single MSR `0x830`, preceded by `mfence; lfence` because x2APIC ICR writes are not serialising. A CPU that the firmware left in x2APIC mode while the BSP runs xAPIC is taken back through the disabled state — OVMF does exactly this with more than 255 vCPUs, and those APs' timers died about a second into the run until `75453c1`.

`send_ipi` takes a dense index and translates it to the APIC ID; the xAPIC two-register sequence runs with IRQs masked so an interrupt handler sending its own IPI in between cannot retarget it.

### SMP start-up and x2APIC IDs

CPUs come from the ACPI MADT (Local APIC and Local x2APIC entries with Enabled set, deduplicated, BSP excluded; disabled and hot-plug entries are counted, never started). The trampoline is copied to physical page `0x7000` (kept out of the buddy allocator with the rest of the first megabyte); its GDT/GDTR sit at `+0xE00`, and a size check refuses SMP rather than let code and GDT overlap. Start-up is **targeted**: INIT to every target, one 10 ms wait, then per AP publish its stack and APIC ID, SIPI, ~200 µs, SIPI, and wait for it to count itself online. The trampoline computes its own APIC ID from CPUID (leaf 0xB EDX, else leaf 1 EBX[31:24]) and claims the parameter block with `lock cmpxchg` only if the published ID is its own; the BSP fences off a non-responder with the same `cmpxchg`, so a late riser parks in `cli; hlt` instead of sharing the next AP's stack. The trampoline enables `EFER.NXE` together with `LME`, because NX bits in the page tables are reserved until then; it sets only `PE` and `PG` in `CR0`. In `sched_ap_entry` each AP adopts the BSP's PAT first, then runs the same `enable_sse` as the BSP — which is where `CR0.WP` is set (until 2026-09-27 APs ran with `WP=0`, so kernel stores into CoW and read-only user pages silently succeeded and ten `uptrtest` CoW cases failed on x86-64 only) — then initialises its APIC, GDT/TSS, SYSCALL MSRs and local timer, and parks until the scheduler opens. Rationale and the topology matrix: [scheduler.md](scheduler.md#dense-cpu-indices-decided-at-bring-up).

SMT topology (`arch_core_of`) is derived from the APIC ID shifted by the SMT width from CPUID, so the idle-CPU picker can prefer a whole idle core.

### PAT, identical on every CPU

`PageFlags::WRITECOMBINE` must mean WC. `IA32_PAT` is per logical processor, and an AP leaves INIT/SIPI with the reset PAT.

- **The finding that framed the work:** Limine 11.4.1 programs `IA32_PAT` itself, to `0x0000_0105_0007_0406` — PA4 = WP, **PA5 = WC**, PA6/PA7 = UC — decoded statically from `BOOTX64.EFI` and confirmed at runtime on two machines (`[ARCH] IA32_PAT before=0x0000010500070406 after=0x0000010500070406 wc=1`). The direct-boot path leaves the reset PAT.
- **Slot choice: PA5**, selected by a leaf PTE with `PAT=1, PCD=0, PWT=1`. The BSP's write is a read-modify-write of byte 5 only, which is value-identical under Limine (so it cannot reinterpret any live translation, including Limine's own WC framebuffer mapping) and changes an unused slot on direct boot. Linux's choice, PA1, was rejected because PA1 is selected by `PWT` alone and the kernel inherits Limine's page tables wholesale: an inherited write-through mapping could silently become WC.
- **Every AP adopts the BSP's whole 64-bit PAT** verbatim, as the first statement of `sched_ap_entry`. This closed a real, live divergence: Limine's 2 MiB framebuffer mapping selects PA5, which was **WC on the BSP and WT on every AP** — the same physical lines under two memory types, which the SDM leaves undefined. The kernel's attempt to re-map the framebuffer uncached failed on 1536 of 2025 pages because `map_4k` cannot split Limine's huge pages. It had not bitten only because WT is coherent and the console idempotent.
- **Change protocol:** IF off, `CR4.PGE` off, `wbinvd`, `wrmsr`, `wbinvd`, `mov cr3, cr3`, restore. The no-fill-cache-mode step (`CR0.CD`) is omitted: no slot that changes has a live user at that instant, and under KVM `CR0.CD` is an MMU-wide memory-type event that adds risk.
- **Fallback:** if the CPU does not confirm WC (`wc=0`), `WRITECOMBINE` maps as PCD (UC-), never WB.
- **The PAT bit is bit 7 in a 4 KiB PTE and bit 12 in a 2 MiB PDE.** Handled structurally: `map_4k` only ever writes leaves at the PT level, and `ensure_table` masks intermediate entries to `PRESENT|WRITABLE|USER`, so the PAT bit can never leak into a level where bit 7 means "huge page". `PAT_4K` is an explicit alias of `HUGE` with that invariant stated.

Measured benefit: a 1 MiB `memcpy` into a host-visible virtio-gpu blob, WC median 59.0 µs against UC- 6324.8 µs (n=30 each, disjoint distributions) — 107×. **Caveat:** a plain host blob answers "cached", so only buffers the host asks to be uncached (today, Mesa's fence-feedback slot) take the WC path; a naive benchmark never touches it. MTRRs cannot defeat this on the verification host (AMD NPT uses the guest PAT; Linux's own `pci_ioremap_wc_bar` relies on MTRR-UC + PAT-WC = WC). The console's use of WC is described in [graphics/console.md](../graphics/console.md).

### TLB shootdown

`LOADED_ROOT[cpu]` is published at every CR3 write, ordered against the PTE store by a fence (Dekker style). `arch_tlb_flush_range(root, va, pages)` does a local `invlpg` and sends vector 0xFD **only to CPUs that currently have `root` loaded** — none at all for a single-threaded process. Acknowledgements are per-CPU flush generations, not a global lock; the initiator's wait is bounded (200 k spins), and every IRQ-masked spin loop in the kernel — on an address space's `busy` flag, on a `TrackedMutex`, or waiting for an ack — services pending flushes itself, because a target spinning with IRQs masked would otherwise never take the IPI. The ICR write is IRQ-safe. Numbers are in [memory.md](memory.md#tlb-shootdown).

### Timer and TSC

The LAPIC timer gives each CPU a 100 Hz tick on an absolute TSC grid, plus one-shot countdowns for the per-CPU deadline timer (never a shortened period: QEMU mishandles that). `timer::init` resolves the TSC frequency once, in order of preference: the hypervisor timing leaf `0x40000010`; CPUID `0x15` with the crystal enumerated; `0x15` × `0x16` with the implied crystal snapped to a standard part; a PIT channel-2 measurement (three 10 ms windows, shortest kept, each bounded so a board without an 8254 cannot hang the boot). A stated value wins when the PIT agrees within 5 %; otherwise the measurement wins. It is printed once as `[TSC] <MHz> (<source>)`. Every kernel clock, including the driver-side microsecond clock, derives from it; the old `_rdtsc() / 1000` assumed 1 GHz and made audio stall thresholds 1.9–4.5× too short (lane tsccal, 2026-09-18). Clock semantics are in [time.md](time.md).

### Interrupt vectors, MSI and MSI-X

Every vector 0x21–0xEF except 0x40 enters a 16-byte stub, `push v; jmp irq_dyn_common`, which saves caller-saved registers, does `swapgs` for an interrupt from ring 3, and calls `irq_dyn_dispatch(v)`: **EOI first, centrally**, then `irq_enter` → handler → `irq_exit`. Vectors are allocated from 0x41 upward (`drivers::irq::alloc_vector`); an unregistered vector is counted and logged once. MSI messages target the LAPIC window `0xFEE0_0000` with the **BSP's** APIC ID (not 0 — it usually but not necessarily is 0), fixed delivery, edge; a BSP APIC ID above 255 is refused because it would need interrupt remapping. `MsixTable` programs per-entry vectors and masks; virtio-gpu's control-queue vector comes from the allocator (`[GPU] MSI-X armed: control queue -> vector 0x41`). Driver-facing API: [hardware/drivers.md](../hardware/drivers.md).

### PS/2 keyboard

The i8042 is probed (and ruled out when the FADT says there is none) before IRQ 1 is trusted, and re-checked after the xHCI BIOS hand-off ends the firmware's USB legacy emulation, because a real PC's "i8042" may be nothing but that emulation. See [input.md](../input.md) and [hardware/pc.md](../hardware/pc.md).

## Design decisions

### `arch_cpu_id()` through `RDTSCP`, not the LAPIC

`current_pid()` calls `arch_cpu_id()` on every syscall path. Reading the LAPIC ID over uncacheable MMIO cost ~100 ns per call and **12.5 % of all running guest CPU** on a COSMIC desktop (2026-08-10); it was not a VM exit (AVIC was enabled) but uncacheable-load latency, located at the instruction after the load (long-latency skid). `IA32_TSC_AUX` is seeded with the dense index inside `apic::init` — the one init both the BSP and every AP run, after the PAT adoption — and read back with `RDTSCP`; the table lookup is the fallback when CPUID lacks RDTSCP.

### MADT enumeration, not Limine's MP feature

Limine can start the APs itself. The kernel does not request that: Limine's AP stacks and info structures live in bootloader-reclaimable memory, and the MADT path also serves the multiboot2/PVH entry. No MADT means uniprocessor, never a broadcast INIT/SIPI.

### FXSAVE, no XSAVE

Context switches save the 512-byte `FXSAVE64` area. `CR4.OSXSAVE` is never set, so CPUID reports no OS support for AVX state and userspace libraries stay on SSE code paths; enabling AVX would require XSAVE-area management in the context switch and signal frames.

## Limitations and open work

- **No SMAP/SMEP** (`CR4` bits 20/21 are never set) — see [memory.md](memory.md#smap-is-a-discipline-here-not-a-hardware-feature). `CR0.WP` is on.
- No AVX/AVX-512 state management (above).
- MSI always targets the BSP; no interrupt affinity, no multi-vector MSI (MSI-X has per-entry vectors). Legacy INTx (`IrqSource::Line`) returns `Unsupported`: there is no ACPI `_PRT` parser.
- More than 64 CPUs would need a wider shootdown acknowledgement mask; firmware-enabled x2APIC on the *BSP* is handled by code but has not been exercised (OVMF hands the BSP over in xAPIC mode even with 288 vCPUs).
- KVM pvclock would give the host's exact TSC frequency without a PIT window; at 0.02 % error it has not been needed. Bare-metal CPUID `0x15`/`0x16` paths are unverified on real hardware.

## Code and tests

Code: [arch/x86_64/src](../../arch/x86_64/src). The x86-64 kernel target is [targets/x86_64-unknown-kernel.json](../../targets/x86_64-unknown-kernel.json) (code model `kernel`, red zone disabled, soft-float).

Tests: `uptrtest` (CoW and read-only kernel stores, `EFAULT` fixups), `forktest` and `memtest` (shootdowns, CoW), `timertest`'s `clock_monotonic_tsc_scale` (raw `rdtsc` over ~300 ms of `CLOCK_MONOTONIC` must match `/proc/cpuinfo`'s `cpu MHz` within 1 %), `ktest=msi` with QEMU's `edu` device (MSI) and a second `virtio-rng-pci` (MSI-X deliver / mask + PBA pending / unmask), and the `regress` suite under a sparse topology (`LEANDROS_SMP=6,sockets=2,cores=3`). Boot lines to check: `[ARCH] IA32_PAT … wc=1`, `[TSC] …`, `[SMP] …`.

## See also

- [arch-aarch64.md](arch-aarch64.md) — the other architecture
- [scheduler.md](scheduler.md) — dense CPU indices, IPIs, the watchdog
- [memory.md](memory.md) — page tables, cache types, shootdowns
- [hardware/pc.md](../hardware/pc.md) — the first real-PC boots
- [graphics/console.md](../graphics/console.md) — WC framebuffer console
