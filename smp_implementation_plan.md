# SMP Support Implementation Plan

This plan details the changes required to implement full, safe Symmetric Multiprocessing (SMP) support in LeandrOS. Currently, SMP bringup code exists but is not invoked during boot, local timers and interrupt routing are not enabled on secondary CPUs, the scheduler contains unsafe single-CPU assumptions, and cross-core synchronization primitives like reschedule IPIs and TLB shootdowns are missing.

## User Review Required

> [!IMPORTANT]
> - **Limine Revision 6 Compatibility**: All changes will conform to Limine Revision 6 bootloader specification mandates.
> - **Lost Wake-up in Futexes**: The current `sys_futex` reads user memory without holding the `FUTEX_TABLE` lock. On SMP, this is highly prone to lost wake-ups because another core can modify memory and call `futex_wake` before the waiting core registers itself. Serializing `*uaddr == val` under the `FUTEX_TABLE` lock is required.
> - **Shared Runqueue Race**: The cooperative scheduler allows CPUs to run concurrently. Adding `on_cpu` tracking ensures we don't accidentally schedule the same task on two cores before its context has finished saving.

## Open Questions

None at this stage. The architecture and scheduling interfaces have clear extension points that fit this design naturally.

## Proposed Changes

### Component: Scheduler (`sched`)

Summary: Address race conditions and global state assumptions.
- Add `on_cpu` tracking to task structures to prevent double scheduling.
- Make preemption flags per-CPU.
- Unify cross-core wake-ups and reschedule notifications using architecture-dependent IPIs.

---

#### [MODIFY] [lib.rs](file:///Users/forain/code/leandros/sched/src/lib.rs)
- Modify `PREEMPT_NEEDED` to be an array of `AtomicBool` indexed by CPU ID.
- Adjust `timer_tick_irq()` and `preempt_check()` to read/write only the calling CPU's slot in `PREEMPT_NEEDED`.
- Implement `pub fn trigger_preempt(cpu: usize)` to set `PREEMPT_NEEDED[cpu] = true` and call `arch_send_resched_ipi(cpu)`.
- Implement `wake_up_an_idle_cpu()` to find an active CPU with `CURRENT_PID[id] == 0` and send it a reschedule IPI.
- Invoke `wake_up_an_idle_cpu()` when tasks are enqueued or unblocked: in `spawn()`, `spawn_user_with_address_space()`, `unblock_port()`, and `deliver_signal()`.
- In `scheduler_run_loop()`, set `t.on_cpu = Some(id)` when picking a task, and clear it (`t.on_cpu = None`) in the scheduler context when context switching finishes.
- Declare the following external interfaces:
  ```rust
  extern "C" {
      fn arch_send_resched_ipi(cpu: usize);
      fn arch_active_cpu_count() -> usize;
  }
  ```

#### [MODIFY] [runqueue.rs](file:///Users/forain/code/leandros/sched/src/runqueue.rs)
- Update `pick_next()` to skip any tasks that have `task.on_cpu.is_some()`.

#### [MODIFY] [task.rs](file:///Users/forain/code/leandros/sched/src/task.rs)
- Add `pub on_cpu: Option<usize>` field to `Task` (initialized to `None`).

#### [MODIFY] [futex.rs](file:///Users/forain/code/leandros/sched/src/futex.rs)
- Update `futex_wait` to accept `expected_val` and validate the user-space futex value under the `FUTEX_TABLE` lock before blocking. Drop the lock prior to context switching.

---

### Component: Core Kernel (`kernel`)

Summary: Wire up boot-time SMP initialization and adjust system calls.

---

#### [MODIFY] [syscall.rs](file:///Users/forain/code/leandros/kernel/src/syscall.rs)
- Update `sys_futex` to pass `val` as `expected_val` to `sched::futex_wait` rather than validating it unsafely beforehand.

#### [MODIFY] [main.rs](file:///Users/forain/code/leandros/kernel/src/main.rs)
- Ensure all architectures start their secondary CPUs before spawning the userspace init task.

---

### Component: x86-64 Architecture Support (`arch/x86_64`)

Summary: Call `smp_init`, configure AP local timers without re-calibration, define vector handlers for reschedule and TLB shootdown IPIs, and implement the TLB shootdown synchronization protocol.

---

#### [MODIFY] [lib.rs](file:///Users/forain/code/leandros/arch/x86_64/src/lib.rs)
- Invoke `smp::smp_init(7)` at the end of `init()`.
- Update `cpu_id()` to call `smp::arch_cpu_id()`.

#### [MODIFY] [timer.rs](file:///Users/forain/code/leandros/arch/x86_64/src/timer.rs)
- Save the calibrated `ticks_per_irq` value in a global `TICKS_PER_IRQ` variable.
- Implement `pub unsafe fn init_local_timer()` to program the Local APIC timer using `TICKS_PER_IRQ` directly.

#### [MODIFY] [smp.rs](file:///Users/forain/code/leandros/arch/x86_64/src/smp.rs)
- Maintain a global `ACTIVE_CPUS` atomic counter. Increment it inside `sched_ap_entry()` when secondary CPUs boot.
- Implement `arch_active_cpu_count()` to return `ACTIVE_CPUS` value.
- Implement `arch_send_resched_ipi(cpu: usize)` using the Local APIC's ICR register to send interrupt vector `0x40`.
- In `sched_ap_entry()`, call `timer::init_local_timer()` so APs receive timer ticks.

#### [MODIFY] [idt.rs](file:///Users/forain/code/leandros/arch/x86_64/src/idt.rs)
- Define Reschedule IPI (vector `0x40`) handler which signals EOI and triggers preemption.
- Define TLB Shootdown IPI (vector `0xFD`) handler which flushes the calling CPU's TLB and decrements the global acknowledgement counter.
- Register these handlers in the Interrupt Descriptor Table (IDT).

#### [MODIFY] [paging.rs](file:///Users/forain/code/leandros/arch/x86_64/src/paging.rs)
- Implement `arch_tlb_shootdown_all()`:
  1. Reload CR3 on the current CPU.
  2. If `arch_active_cpu_count() > 1`, serialize with a spinlock and set the acknowledgement counter to `active_cpus - 1`.
  3. Broadcast TLB Shootdown IPI (vector `0xFD`, shorthand `all-excl-self`).
  4. Spin-wait until all target CPUs acknowledge.

---

### Component: AArch64 Architecture Support (`arch/aarch64`)

Summary: Call `smp_init`, route GIC timer PPI 27 on secondary CPUs, enable virtual timers on APs, and implement SGI 1 for reschedule notifications.

---

#### [MODIFY] [lib.rs](file:///Users/forain/code/leandros/arch/aarch64/src/lib.rs)
- Invoke `smp::smp_init(&[1, 2, 3, 4, 5, 6, 7])` at the end of `init()`.

#### [MODIFY] [gic.rs](file:///Users/forain/code/leandros/arch/aarch64/src/gic.rs)
- Modify `init_cpu_interface()` to enable PPI 27 (virtual timer) in the banked `GICD_ISENABLER0` register for each secondary CPU.
- Implement `send_sgi(cpu: usize, sgi_id: u32)` using `GICD_SGIR` to route interrupts to specific cores.
- Implement `arch_send_resched_ipi(cpu: usize)` using `send_sgi(cpu, 1)`.

#### [MODIFY] [smp.rs](file:///Users/forain/code/leandros/arch/aarch64/src/smp.rs)
- Maintain a global `ACTIVE_CPUS` atomic counter. Increment it in `aarch64_sched_ap_entry()`.
- Implement `arch_active_cpu_count()` to return `ACTIVE_CPUS`.
- In `aarch64_sched_ap_entry()`, call `super::timer::init()` to start virtual timers and enable interrupts.

#### [MODIFY] [exception.rs](file:///Users/forain/code/leandros/arch/aarch64/src/exception.rs)
- Add a handler block for SGI 1 (Reschedule IPI) inside `handle_irq()`.

---

## Verification Plan

### Automated Tests
We will build the kernel for both targets to verify compilation:
- `cargo check -p kernel --target=targets/x86_64-unknown-kernel.json -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec`
- `cargo check -p kernel --target=targets/aarch64-unknown-kernel.json -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec`

We will run the existing thread and timer test suite under QEMU for both architectures to ensure no regressions occur:
- `./scripts/build-all.sh`
- `./scripts/run-qemu.sh`

### Manual Verification
- Verify kernel logs during boot show that secondary CPUs are successfully booted and start their scheduler loops:
  - On x86_64: `smp::smp_init` boots secondary cores.
  - On AArch64: `smp::smp_init` calls `cpu_on`.
- Boot QEMU with multiple cores (`-smp 4` or `-smp 8`) and inspect console output.
