# lane/cpuids — dense CPU indices, targeted AP start-up, x2APIC (2026-10-07)

Branch `lane/cpuids` from `72b1a70`.

## The problem

Found while decoding the photo of the first real-PC boot (x86_64 UEFI, see
`lane-usbhid-2026-10-07.md` "What the watchdog was saying"): the stall
watchdog reported `cpu1`/`cpu3` with `local_ticks=0 pid=0` from the start.

* x86_64 `arch_cpu_id()` returned the **raw LAPIC ID** and every per-CPU
  array (`MAX_CPUS = 8`) was indexed by it, while `active_cpu_count()`
  counted started APs. Real machines have sparse IDs (0,2,4,6 on a 4-core
  part without SMT; AMD multi-CCX, hybrid Intel and two-socket boxes leave
  gaps; x2APIC IDs exceed 255), so slots belonged to no CPU and CPUs with an
  ID ≥ 8 clamped onto another CPU's slot.
* `smp_init` woke **every** AP with a broadcast INIT/SIPI/SIPI but had stacks
  for 7: on a machine with more than 8 threads the extra APs ran with a stack
  pointer read from past the array.
* aarch64 had the same shape: `arch_cpu_id()` = `MPIDR.Aff0`, and APs were
  "enumerated" as the hard-coded MPIDR list 1..7. On a Pi 5 (A76: Aff0 = 0,
  core in Aff1) every core has index 0; on QEMU `virt` with more than 16
  vCPUs the 17th has MPIDR 0x100.

Reproduced in QEMU before the fix: `-smp 6,sockets=2,cores=3` (APIC IDs
0,1,2,4,5,6) gives `[WDOG] cpu3 took no timer tick ... local_ticks=0` every
10 s — the photo's symptom exactly.

## Design

### Dense index

`sched::MAX_CPUS` = **64** (was 8). It sizes every per-CPU table; per-slot
static cost is a few KiB (x86 4 KiB double-fault stack, scheduler context,
512 B timer/TLB state, the watchdog's 64×64 matrix = 32 KiB total). 64 is
also the width of the x86 TLB-shootdown ack mask (`u64`), now asserted.
Processors beyond it are left parked and logged.

Index 0 is the BSP. APs get the **next index only when they actually come
up**, one at a time, so the online CPUs are always exactly
`0..active_cpu_count()` — an AP that does not answer gives its index back.

### x86_64 (`arch/x86_64/src/cpus.rs`, `smp.rs`, `apic.rs`)

* Enumeration: ACPI MADT Local APIC (type 0) and Local x2APIC (type 9)
  entries with Enabled set, table order, deduplicated by ID, BSP excluded
  (`boot::acpi::for_each_madt_cpu`). Disabled / online-capable (hot-add)
  entries are counted, never started. No MADT ⇒ uniprocessor (no broadcast
  fallback). Limine MP was considered and not used: the kernel does not
  request it, its AP stacks and info structs live in bootloader-reclaimable
  memory, and the MADT path also serves the multiboot2 entry.
* Start-up: targeted INIT to every target, one 10 ms wait, then per AP:
  publish stack + APIC ID in the parameter block, SIPI, ~200 µs, SIPI. The
  trampoline computes its own APIC ID from CPUID (leaf 0xB EDX, else leaf 1
  EBX[31:24]) and claims the block with `lock cmpxchg` only if the published
  ID is its own; the BSP fences off a non-responder with the same cmpxchg,
  so a late riser parks in `cli; hlt` instead of sharing the next AP's stack.
  The BSP waits for each AP to count itself online before the next.
  The trampoline also sets EFER.NXE with LME (NX bits are reserved until
  then). GDT/GDTR moved to +0xE00 in the trampoline page; a size check
  refuses SMP rather than overlap them.
* `arch_cpu_id()` still reads `IA32_TSC_AUX` via `RDTSCP` (no extra cost on
  the hot path — see the `current_pid()` LAPIC-read history), but the value
  is now the dense index, seeded from `cpus::index_of(own_apic_id())`.
  Fallback without RDTSCP: table lookup.
* Index → APIC ID translation everywhere an ID is an *address*: resched and
  TLB-shootdown IPIs (`apic::send_ipi`), `arch_core_of` (SMT siblings from
  the APIC ID, not the index), IOAPIC IRQ 1 and MSI destination (the BSP's
  APIC ID, not 0; MSI refuses a BSP ID > 255, which would need interrupt
  remapping).
* x2APIC: register access moves to MSRs 0x800+ when the firmware handed over
  in x2APIC mode or an enabled CPU we start has an ID > 254 (and CPUID says
  x2APIC). The BSP decides; every AP follows. ICR is the single MSR 0x830
  with an `mfence; lfence` before it (x2APIC ICR writes are not
  serializing). A CPU the firmware left in x2APIC mode while the BSP runs
  xAPIC is taken back through the disabled state — OVMF with > 255 vCPUs
  does exactly this, and those APs' timers died a second into the run.

### aarch64 (`arch/aarch64/src/smp.rs`, `gic.rs`)

* Enumeration: MADT GICC entries (Enabled), else device tree `/cpus/cpu@*`
  (`device_type = "cpu"`, status okay; `boot::device_tree::for_each_cpu_mpidr`
  on the DTB `parse` recorded), else the old Aff0 1..7 guess.
* PSCI CPU_ON one at a time; the index is assigned before CPU_ON and taken
  back if CPU_ON fails (ALREADY_ON now counts as a failure: the CPU never
  runs our stub); the BSP waits for each AP to come online.
* `arch_cpu_id()`: `mrs mpidr_el1` + one byte load from a 1024-entry table
  keyed by Aff0[3:0] | Aff1[3:0]<<4 | Aff2[1:0]<<8 (linear search only if two
  CPUs collide in that key — none known). Zero-initialised, so everything
  maps to 0 before `smp_init`, when only the BSP runs.
* `AP_BOOT_PARAMS` is cleaned to PoC before CPU_ON (the stub reads it with
  the MMU and caches off; QEMU does not care, real cores do).
* GICv2 SGIs use each CPU's own interface mask (banked `GICD_ITARGETSR0`
  read) instead of `1 << index`; SPIs target the BSP's recorded mask.
  GICv3 already addressed by MPIDR. `GICR_MAX_FRAMES` = `MAX_CPUS`.

`driver.py` honours `LEANDROS_SMP` on aarch64 too.

## QEMU topology matrix (Mac; x86_64 TCG, aarch64 HVF)

| arch | topology | APIC IDs / MPIDRs | result |
|---|---|---|---|
| x86_64 | before: `6,sockets=2,cores=3` | 0,1,2,4,5,6 | **`[WDOG] cpu3 … local_ticks=0` ×12** (bug reproduced) |
| x86_64 | `6,sockets=2,cores=3` | 0,1,2,4,5,6 → idx 0..5 | 6 online, no WDOG, login |
| x86_64 | `10,sockets=2,cores=5` | 0-4, 8-12 | 10 online, no WDOG, login |
| x86_64 | default `4,sockets=1,cores=2,threads=2` | 0-3 | 4 online, no WDOG, login |
| x86_64 | `8,sockets=1,cores=4,threads=2` | 0-7 | 8 online, no WDOG, login |
| x86_64 | `12,sockets=3,cores=2,threads=2` | 0-11 | 12 online, no WDOG, login |
| x86_64 | `16` | 0-15 | 16 online, no WDOG, login |
| x86_64 | `4,maxcpus=8` | 0-3 (4 hot-plug slots disabled) | 4 online, no WDOG, login |
| x86_64 | `64` | 0-63 | 64 online, login; a few transient 2–12 s WDOG lines (64 TCG vCPUs on the Mac, recovered) |
| x86_64 | `70` | 0-69 | 64 online, 6 parked + logged, login; one transient 2 s WDOG |
| x86_64 | `128` | 0-127 | 64 online, 64 parked, login; two transient 2 s WDOG |
| x86_64 | `4,sockets=2,cores=256,maxcpus=512` + cold-plugged socket-1 CPUs, intel-iommu eim | 0,1,2,3,**259,263** | kernel switches to **x2APIC**, 6 online, no WDOG, login |
| x86_64 | `288` + intel-iommu eim | 0-287 | first run: APs left in x2APIC by OVMF went silent after ~1 s (fixed in `75453c1`); after: 64 online, 224 parked, login, two transient WDOG lines |
| aarch64 | default `-smp 4` | MPIDR 0-3 (MADT) | 4 online, no WDOG, login |
| aarch64 | `-smp 8` | 0-7 | 8 online, no WDOG, login |
| aarch64 | `-smp 20` | 0x0-0xf, **0x100-0x103** | 20 online, no WDOG, login |
| aarch64 | `-smp 20`, `-machine acpi=off` | same, from the **device tree** | 20 online, no WDOG, login |

Regression and desktop (at `75453c1`, kernel-only rebuild via
`scripts/m7z2-kernel-only.sh` on top of the main checkout's userland,
initrd and data images):

* `runtests.py x86_64 --suite regress`, default topology: **15/15 PASS**, no WDOG.
* `runtests.py x86_64 --suite regress`, `LEANDROS_SMP=6,sockets=2,cores=3`
  (sparse APIC IDs): **15/15 PASS**, no WDOG.
* `runtests.py aarch64 --suite regress`, `LEANDROS_SMP=8` (HVF): **15/15 PASS**, no WDOG.
* `ffsession.py --nofirefox --gpu virgl`: greeter → COSMIC desktop →
  cosmic-term on x86_64 (default topology, TCG) and aarch64 (`-smp 8`, HVF),
  screenshots fine, no WDOG.

## Not covered / follow-ups

* Pi 5 hardware: SMP now actually starts there (the old 1..7 list failed
  CPU_ON with INVALID_PARAMS, so it ran single-core by accident). Untested on
  the board; the stub's MMU-off read of `AP_BOOT_PARAMS` is cleaned to PoC
  now, but watch the first Pi 5 boot's `[SMP]` line.
* `kernel/src/entry_aarch64.s`'s direct-boot park loop still elects on
  `MPIDR.Aff0` (raspi4b path only).
* Firmware-enabled x2APIC on the BSP is handled by code but not exercised:
  OVMF hands the BSP over in xAPIC mode even with 288 vCPUs.
* `sys_getcpu` still reports CPU 0 (it could now return `cpu_id()`).
* More than 64 CPUs would need a wider TLB ack mask (and `MAX_CPUS` > 256 a
  wider key table entry on aarch64).
