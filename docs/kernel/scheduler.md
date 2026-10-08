# Scheduler and SMP

The `sched` crate runs every task in the system — user threads, the kernel's own tasks and kernel threads — from **one global run queue** with an EEVDF policy, served by up to 64 CPUs. This page covers the run queue and dispatch, preemption and blocking, the context switch and the FP/SIMD policy, CPU bring-up and dense CPU indices, idle, affinity, the `Stopped` state, the tick watchdog and lock profiling, and the kernel-thread facilities. Processes, signals, futexes and timekeeping have their own pages ([processes.md](processes.md), [signals.md](signals.md), [threads-and-futex.md](threads-and-futex.md), [time.md](time.md)).

## Overview

| Item | Value | Where |
|---|---|---|
| Policy | EEVDF (Earliest Eligible Virtual Deadline First), nice −20..19 → Linux's weight table | [sched/src/runqueue.rs](../../sched/src/runqueue.rs), [sched/src/task.rs](../../sched/src/task.rs) |
| Run queue | One, shared by all CPUs, under the `RUN_QUEUE` `TrackedMutex` | [sched/src/lib.rs](../../sched/src/lib.rs) |
| Tasks (threads) | `MAX_TASKS` = 1024 | `runqueue.rs` |
| Processes (thread groups) | `MAX_PROCESSES` = 512 | `runqueue.rs` |
| CPUs | `MAX_CPUS` = 64 (dense logical index 0 = BSP) | `sched/src/lib.rs` |
| Tick | 100 Hz per CPU, on an absolute grid, plus a per-CPU one-shot deadline timer | arch timers, [time.md](time.md) |
| Slice | `BASE_SLICE_TICKS` = 4 ticks per deadline period (nice 0) | `task.rs` |
| Kernel stack | 128 KiB per task (`KERNEL_STACK_ORDER` = 5), canary-checked | `sched/src/lib.rs` |
| User stack | 8 MiB main-thread stack, demand-paged | [memory.md](memory.md#user-address-space-layout) |
| Task states | `Ready`, `Running`, `Blocked`, `Stopped`, `Zombie` | `task.rs` |

## How it works

### The run queue and EEVDF

[sched/src/runqueue.rs](../../sched/src/runqueue.rs). Every runnable task carries a weight (from its nice value via Linux's `sched_prio_to_weight` table; one nice level ≈ 1.25× CPU share), a `vruntime` that advances by `ticks × VT_PER_TICK × NICE0_WEIGHT / weight` while it runs, and a `vdeadline = vruntime + slice/weight` renewed when it expires. A task is **eligible** when its `vruntime` is at or below the weighted average of the runnable set; `pick_next` returns the eligible task with the earliest virtual deadline. This is the same family as Linux ≥ 6.6's default scheduler. (Older documentation described "cooperative + preemptive round-robin with signed priority"; the signed priority survives as the nice value.)

The queue is a fixed array of `Option<Box<Task>>` slots with three accelerators, each of which can cost time but can never return a wrong answer:

- an **occupancy bitmap**, so scans skip empty slots;
- a **`maybe_ready` superset bitmap**, set whenever a `&mut Task` is handed out (the only way a task can become runnable — `tasks` is private so nothing bypasses it) and cleared only by `pick_next`; once per tick `pick_next` does a full scan that repairs it and counts misses (`ready_hint_miss`, 0 in every measured run);
- a **pid → slot hint table** (4096 entries, 8-slot probe window), verified against the slot on every hit, with a linear scan as fallback.

On the idle COSMIC desktop `pick_next` visits about 41 tasks instead of 256, at 873–1039 ns instead of ~1.85 µs (lane runqlock, 2026-09-25).

### Dispatch

Each CPU runs `scheduler_run_loop` on its own static scheduler context:

1. Open an `irq_window()` — the systemic guarantee that a CPU hosting only kernel wait loops still takes its local tick.
2. Under one `RUN_QUEUE` hold: `pick_next`, set `on_cpu = Some(cpu)` and `Running`. The `on_cpu` claim guards against double dispatch: a task stays claimed until its CPU has switched back and its saved registers are complete.
3. Publish the per-CPU identity — `CURRENT_PID`, `CURRENT_TGID`, `CURRENT_REPLY_PORT`, `CURRENT_AS` — set the kernel stack (TSS `rsp0` / per-CPU syscall stack on x86-64), load the task's page table, and `cpu_switch_to` the task.
4. On switch-back: clear the per-CPU identity, **detach from the task's page table** (`arch_load_kernel_page_table`: reload the boot CR3 / clear TTBR0) so a freed table is never live on any CPU, then, under one `RUN_QUEUE` hold, charge vruntime and CPU time, release `on_cpu`, park a thread whose group was stopped meanwhile, and if the task is a Zombie log its exit status to `EXIT_LOG` *before* removing its slot (so a waiter on another CPU always finds it in one place or the other) and free its kernel stack.
5. Nothing to run: `sti; hlt; cli` / `msr daifclr, #2; wfi; msr daifset, #2`.

### Preemption and the kernel's interrupt model

The kernel runs **with interrupts masked from syscall entry to syscall exit** (and in fault handlers). Every CPU's tick sets its own `PREEMPT_NEEDED`; `preempt_check` acts on it at the return to user mode, or wherever kernel code opens a window. Remote wake-ups go through `trigger_preempt`, which sends the architecture's reschedule IPI (x86-64 vector 0x40, AArch64 SGI 1).

Kernel code is therefore preemptible only at explicit points:

- `yield_now` — a full switch to the scheduler context;
- a blocking primitive's commit (below);
- `irq_window()` — `sti; pause; cli` / `msr daifclr, #2; nop; msr daifset, #2`, which lets a pending tick in. Because the tick calls `preempt_check`, an `irq_window` **is a scheduling point**.

`preempt_disable()`/`preempt_enable()` (per-CPU nesting counter) separate the two: inside a disabled section, ticks are still taken and tick hooks still run, but the switch is deferred — the flag is left set, never dropped. This exists for long kernel waits that hold a lock: `virtio_gpu::submit` holds `VIRTIO_GPU` while waiting for the host, and a yield inside it parked the mutex while every other acquirer spun with IRQs masked — a hard hang on a 1-vCPU guest. The corollary, documented on `preempt_disable`, is that **tick hooks must be `try_lock`-only** (a bounded `try_lock_spin` counts) and must never touch `VIRTIO_GPU` or the framebuffer console, because they can run on a CPU that holds those.

### Blocking: the three-phase protocol

Every blocking primitive uses the same lost-wake-proof sequence ([sched/src/lib.rs](../../sched/src/lib.rs)):

1. `block_on_port_prepare(channel)` — publish `Blocked` on a wait channel **before** checking the condition.
2. Re-check the condition; if it is already satisfied, `block_on_port_cancel()` and proceed.
3. `block_on_port_commit()` — yield. A wake that raced in between already flipped the task back to `Ready`; the scheduler just re-dispatches it.

Publishing `Blocked` while still running is safe because IRQs are masked and `on_cpu` prevents another CPU from dispatching the task before it actually enters the scheduler. Wait channels are IPC port ids (`unblock_port(port)` wakes the port's owner) and sentinels outside the port-id range: `POLL_WAIT_CHANNEL` = `0xFFFF_FF01` (poll/select/epoll/wait4/timed waits, with per-waiter hash masks for targeted wakes — [event-loop.md](event-loop.md)), `VFORK_WAIT_CHANNEL` = `0xFFFF_FF02`, and `KWAIT_CHANNEL` = `0xFFFF_FF03` for the kernel's sleeping locks (woken by pid, below). Futexes block on their own slots ([threads-and-futex.md](threads-and-futex.md)).

### Context switch and the FP/SIMD policy

`cpu_switch_to(old, new)` ([sched/src/context.rs](../../sched/src/context.rs)) saves the callee-saved integer registers, the stack pointer and the thread pointer **and the full FP/SIMD state on every switch**:

| Arch | Integer | FP/SIMD |
|---|---|---|
| x86-64 | rbx, rbp, r12–r15, rsp, fs_base | 512-byte `FXSAVE64` area: x87 + XMM0–15 + MXCSR (fresh tasks get FCW `0x037F`, MXCSR `0x1F80`; an all-zero MXCSR would unmask every SSE exception) |
| AArch64 | x19–x30, sp, `tpidr_el0` | q0–q31 (512 bytes) + FPSR + FPCR |

The switch must save vector state because preemption interrupts a thread at an arbitrary instruction where every vector register may be live; the SysV "caller-saved" convention only covers calls the compiler can see. The first symptom was two MAME threads both running SSE `memcpy` and corrupting each other's XMM registers.

The **kernel itself uses no FP/SIMD**. Both kernel targets are soft-float: x86-64 `-mmx,-sse,…,+soft-float`, AArch64 `+v8a,-neon,-fp-armv8` with the `softfloat` ABI ([targets/](../../targets)). Only the context-switch assembly touches vector registers, inside an explicit `.arch armv8-a+fp+simd` bracket on AArch64. This matters because the EL0 trap frame saves no vector state: when the AArch64 kernel was built `+neon`, LLVM lowered bulk copies on the file-backed fault path through `q0` and clobbered the faulting thread's `q0` on exactly 1 in 16 file-backed page touches ([arch-aarch64.md](arch-aarch64.md#a-soft-float-kernel)). No XSAVE/AVX state is managed on x86-64: `CR4.OSXSAVE` is never set, so userspace sees no AVX and libraries fall back to SSE paths.

### CPU bring-up and dense CPU indices

Every per-CPU table in the kernel is indexed by a **dense logical CPU index** `cpu_id()`: 0 is the BSP and the online CPUs are exactly `0..active_cpu_count()`. Hardware IDs are sparse and are only ever used as *addresses* (IPI destinations, MSI targets, SMT sibling detection).

| | x86-64 | AArch64 |
|---|---|---|
| Enumeration | ACPI MADT Local APIC (type 0) and Local x2APIC (type 9) entries with Enabled set, BSP excluded; no MADT ⇒ uniprocessor | MADT GICC entries, else device-tree `/cpus/cpu@*`, else Aff0 1..7 |
| Start-up | Targeted INIT to every target, one 10 ms wait, then per AP: publish stack + APIC ID, SIPI, ~200 µs, SIPI; the trampoline (physical `0x7000`) claims its parameter block with `lock cmpxchg` only if the published ID is its own | PSCI `CPU_ON` one at a time; MMU-off stub programs MAIR/TCR/TTBRs from the BSP's values |
| Index assignment | Next index only when the AP is actually online; a non-responder is fenced off and its index reused | Assigned before `CPU_ON`, taken back if it fails (`ALREADY_ON` counts as failure) |
| `arch_cpu_id()` | `RDTSCP` reads `IA32_TSC_AUX`, seeded with the dense index (table lookup without RDTSCP) | `mrs mpidr_el1` + one byte load from a 1024-entry table keyed by Aff0/Aff1/Aff2 bits |

APs initialise their per-CPU state and park in `sched::ap_entry` until the BSP calls `sched::run()` and opens `SCHED_ONLINE`, preserving the pre-SMP boot order. Processors beyond `MAX_CPUS` are left parked and logged. Details per architecture: [arch-x86_64.md](arch-x86_64.md#smp-start-up-and-x2apic-ids), [arch-aarch64.md](arch-aarch64.md#psci-and-secondary-cpus).

### Lock-free per-CPU identity

`current_pid()` and friends are on every syscall path, so they must not take `RUN_QUEUE`. The dispatch path publishes per-CPU slots for the running task's pid, tgid, reply port and address space; `tgid_of` for *another* pid uses a lock-free open-addressed pid→tgid side table (4096 slots, tombstones on removal) written only under `RUN_QUEUE` ([processes.md](processes.md#thread-groups-and-the-tgid-key)). The address-space slot lets the running task's page faults and mm syscalls take only the address space's `busy` flag ([memory.md](memory.md#address-spaces-and-vmas)).

### Idle-CPU selection

`wake_up_an_idle_cpu` runs whenever new work becomes runnable (spawn, fork/clone, port unblock, futex wake, signal). It prefers an idle CPU whose **whole physical core** is idle (SMT siblings from the x86 APIC ID and CPUID leaf 0xB topology; identity on AArch64), falls back to any idle CPU, and sends it a reschedule IPI.

### Affinity

There is no per-task CPU pinning: the run queue is global and any CPU runs any task. `sched_getaffinity` reports one bit per online CPU; `sched_setaffinity` performs the permission check and otherwise accepts the mask without effect; `getcpu` reports CPU 0.

### The `Stopped` state

`Stopped` is a real task state for POSIX job control: `SIGSTOP`/`SIGTSTP`/`SIGTTIN`/`SIGTTOU` stop the whole thread group, `SIGCONT` resumes it, and `waitpid(WUNTRACED|WCONTINUED)` observes the transitions. A stopped task is not runnable and no wait channel wakes it; only `SIGCONT` (→ `Ready`) or `SIGKILL` (→ `Ready`, then `Zombie`) move it. A thread that was `Blocked` when stopped resumes as a spurious wake and re-parks, because every blocking primitive re-checks its condition. A sibling still on another CPU when the stop is posted gets `stop_pending` and is parked at its switch-back. See [signals.md](signals.md).

### Per-CPU tick watchdog

A CPU that deadlocks on an IRQ-off spinlock, or loops in kernel mode, stops taking its own tick and produces no output. Each CPU counts its own ticks (`LOCAL_TICKS`); every 50 of its own ticks (0.5 s) it compares every other CPU's counter with what it saw last time. A counter frozen for 4 scans (~2 s) is reported once and then every 10 s, on the raw UART, by the lowest-numbered CPU that is still ticking:

```
[WDOG] cpuN took no timer tick for ~S s (IRQs masked or timer dead): pid=P (exe) last syscall 0x..
       preempt_disable=D spinning for LOCK held by cpuM local_ticks=T (seen from cpuK)
[WDOG]   cpuM holds LOCK
```

The silent CPU is also sent a reschedule IPI on every scan: if it is parked in `wfi` with a dead local timer (seen under HVF), the IPI runs its idle loop once and the `irq_window` there re-arms the timer through `arch_timer_check_alive`, which logs `[TIMER] cpuN virtual timer silent … re-armed`. The watchdog stops reporting once `reboot(2)` marks the system down. `[WDOG]` timestamps are tick-derived and were never on the raw TSC scale. Reading these lines is covered in [development/debugging.md](../development/debugging.md).

**`lockwatch`** ([sched/src/lockwatch.rs](../../sched/src/lockwatch.rs)) is what lets the line name the lock. The hot global locks — `RUN_QUEUE`, `PIPEWIRE_STATE`, `FD_TABLES`, `PIPE_RINGS`, `VIRTIO_GPU`, `PORT_TABLE`, `EPOLL`, and the address-space `busy` flag — are `TrackedMutex`es that publish their holder CPU and each CPU's wanted lock (one relaxed byte store on acquire and release). For `RUN_QUEUE` it also records the acquiring source line; with `HOLD_PROFILE` on it keeps a per-site hold histogram, which Ctrl-T prints as an `[RQPROF]` block. A contended `lock()` spins on `try_lock` and services pending TLB flushes, so a waiter can never block a shootdown its holder is waiting for.

**Ctrl-T** (byte 0x14 on the serial console) dumps every task's state, CPU, blocked port or futex word, last syscall and last EL0 frame, a buddy census, the AF_UNIX tables and registered dump hooks ([development/debugging.md](../development/debugging.md)). `idlestat`, `pcsample` and `gdwake` are gated censuses for idle cost, PC sampling from the tick, and poll-wake attribution.

### Kernel threads, sleeping locks and deferred work

Added 2026-10-07 for in-kernel drivers (the NVIDIA OpenRM shim is the first consumer); each has a Rust API and, where needed, an `extern "C"` entry point (`kfacil_*`). The execution contexts are explicit:

| Context | How you get there | May sleep |
|---|---|---|
| hard IRQ | any interrupt handler, IRQ-mode hrtimer callbacks | no |
| task, atomic | holding a `KSpinLock`/`RawSpinLock` or a `TrackedMutex`, or inside `preempt_disable` | no |
| task | syscall, fault handler, kernel thread, work item, worker-mode hrtimer | yes |
| boot | before `sched::run` | no (waits spin) |

- **`irqctx`** — a per-CPU hard-IRQ nesting counter maintained by both architectures' IRQ paths and dropped before the IRQ path may reschedule; `in_irq()`, `can_sleep()`, `might_sleep()`.
- **`ksync`** — `KSpinLock`/`RawSpinLock` (IRQ-safe), `KMutex` (FIFO hand-off), `KRwLock` (writer-preferring), `KSemaphore`, `KWaitQueue`, `KCompletion`. Waiters park on `KWAIT_CHANNEL` and are woken by pid through `kwake`, which only *try*-locks `RUN_QUEUE` (bounded 50 µs spin) and otherwise defers the wake to the next tick, so every wake is IRQ-safe. **Sleeping where it is not allowed does not park**: it logs once per call site (`[KSYNC] sleep in atomic context … spinning instead`) and spins. Plain `spin::Mutex` holders are invisible to that check.
- **`kthread`** — `spawn(name, closure)`, `msleep`/`udelay`, `cond_resched`, stop/join. Kernel threads start with IRQs masked like syscalls; a long compute loop must call `cond_resched()` or the watchdog reports the CPU after 2 s.
- **`workqueue`** — one worker per queue, FIFO, task context; the system queue is the `[kworker]` task started by the init task. Queueing an already-pending item returns false; `flush` from the queue's own worker is detected and ignored.
- **`hrtimer`** — one-shot and periodic timers driven by the per-CPU deadline timer with the tick as fallback; `Irq` or `Worker` callback mode; 128 armed timers system-wide. Measured one-shot lateness: 15–220 µs on AArch64/HVF, 90–2500 µs on x86-64/TCG.

`ktest=all` on the kernel command line runs a self-test for each facility (plus `atomicpool`, `kva`, `pci`, `msi`, `firmware`); 10/10 on both architectures. The full API and context table are in the kfacil lane note.

## Design decisions

### One global run queue, not per-CPU queues

*Choice:* one EEVDF queue under one lock, served by every CPU. *Why:* the workload is a few hundred threads on up to a handful of CPUs, and a single queue makes migration, wake-up placement and the `on_cpu` claim trivially correct; per-CPU queues bring load balancing and migration races. *Evidence that it holds up:* the lock's cost was measured rather than assumed. The "more than 50 % of ticks fail to take `RUN_QUEUE`" premise was **refuted** at 8.2 %, and there was no long holder — 99.9 % of holds were under 1 µs. The cost was *traffic*: ~760 k acquisitions/s on an "idle" desktop, four per synchronous server call. Removing those (a `send` to one's own port skips the wake scan; `call_port` takes a synchronous reply without the park protocol; `current_reply_port` reads a per-CPU slot) took tick lock failures from 8.2 % to 0.05–0.12 % (lane runq, 2026-09-18). Session start was the remaining hot spot — 338 k holds/s, 10.2 % of a CPU held, 18.9 % spent spinning, dominated by page faults looking up their own address space — and the per-CPU `CURRENT_AS` slot plus the pick bitmaps cut it to 38 k holds/s, 0.96 % held, 0.55 % contended (lane runqlock).

### Kernel overhead is measured at the instruction, not guessed

A COSMIC desktop on x86-64/KVM managed one repaint every 3–5 s, and the first suspect was software rendering. RIP sampling put **74.4 %** of running guest time in the kernel. Three causes (2026-08-10): `arch_cpu_id()` read the LAPIC ID over uncacheable MMIO on every call (12.5 % of running CPU — it is now `RDTSCP`); both epoll probe loops walked all 512 interest slots taking a global lock per slot (22.3 %); `tgid_of` took `RUN_QUEUE` and scanned every slot (10.6 %, then fixed with the side table). Frames per second went 0.20 → 1.00 (the content ceiling: a 1 Hz clock), idle headroom 22.1 % → 41.3 %. **A planned per-epoll-instance wait-channel rewrite was refuted by measurement and must not be built:** once the scans were bounded, the global wake channel's own cost measured 0.7 %. Two measurement lessons came with it: RIP sampling counts halted vCPUs (the idle `hlt` looked like a 42–48 % "hot scheduler loop" until excluded), and the LAPIC read was not a VM exit (AVIC was on) but uncacheable-MMIO latency.

### `sched_getaffinity` must report every CPU

It once wrote a mask with only bit 0 set. Rust's `available_parallelism()` counts that mask, so every Tokio, rayon and Mesa thread pool in the system ran single-threaded, and `(while :; do :; done) & sleep 1` wedged brush because its one Tokio worker was pinned by the loop. Kernel preemption, per-CPU pinning and `nanosleep` were each suspected and refuted first. Fixed in `e158783` (2026-09-14); `/proc/cpuinfo` lists the same count, and `nproc` is the quick probe. It changed every performance baseline taken before it.

### Dense CPU indices, decided at bring-up

*Choice:* index every per-CPU table by an index handed out only when a CPU actually comes online, and translate to hardware IDs only where an ID is an address. *Why:* the first real-PC boot photographed `[WDOG] cpu1/cpu3 … local_ticks=0` from the start. `arch_cpu_id()` returned the raw LAPIC ID, real machines have sparse IDs (0, 2, 4, 6 on a 4-core part without SMT; gaps on multi-CCX AMD, hybrid Intel and two-socket boards; x2APIC IDs above 255), so some slots belonged to no CPU and IDs ≥ 8 clamped onto another CPU's slot; a broadcast INIT/SIPI also woke APs the kernel had no stack for. On AArch64 the A76 puts the core number in Aff1, so every Pi 5 core had index 0. Reproduced in QEMU with `-smp 6,sockets=2,cores=3` before the fix; afterwards a topology matrix up to 288 vCPUs (64 online, the rest parked and logged), sparse APIC IDs, x2APIC IDs 259/263 and 20 AArch64 vCPUs with MPIDR `0x100`+ all reach a login (lane cpuids, `963c681`/`75453c1`, 2026-10-07). `MAX_CPUS` went 8 → 64; 64 is also the width of the x86 shootdown acknowledgement mask.

### A 128 KiB kernel stack with a canary, and a build-time frame budget

There is no guard page under a kernel stack: it is buddy memory reached through the HHDM, so an overflow silently overwrites a neighbouring allocation. `f2fs::mount` once built a 155,880-byte `MountState` as a stack temporary on a 64 KiB stack; it corrupted memory on every host but killed only one, and was first mis-bisected to image size. The stack is now 128 KiB, defined once; `scripts/check-stack-frames.py` fails the build for any frame above 48 KiB; and a canary in the lowest word is checked on every syscall return. See [invariants.md](invariants.md#kernel-stacks-are-128-kib-unguarded-and-policed).

## Limitations and open work

- No CPU affinity or pinning; `getcpu` reports 0 (it could now report `cpu_id()`).
- No real-time scheduling classes; `sched_get_priority_max/min` return 0.
- `pick_next` is still O(candidates) per dispatch; fine at today's task counts.
- More than 64 CPUs would need a wider TLB acknowledgement mask (and, on AArch64 beyond 256, a wider index table entry).
- Plain `spin::Mutex` holders are invisible to `might_sleep()`; locks a sleeping path must not hold need to be `KSpinLock` or `TrackedMutex`.
- A latent self-deadlock on `VIRTIO_GPU` exists when a kernel thread prints through the framebuffer console while another CPU initialises virtio-gpu; the console drain should not flush the framebuffer from `arch_serial_putc`. The self-tests log through the raw UART to avoid it.

## Code and tests

| What | Where |
|---|---|
| Dispatch, blocking, preemption, watchdog, per-CPU state | [sched/src/lib.rs](../../sched/src/lib.rs) |
| Run queue and EEVDF | [sched/src/runqueue.rs](../../sched/src/runqueue.rs), [sched/src/task.rs](../../sched/src/task.rs) |
| Context switch | [sched/src/context.rs](../../sched/src/context.rs) |
| Lock tracking and profiles | [sched/src/lockwatch.rs](../../sched/src/lockwatch.rs), [sched/src/idlestat.rs](../../sched/src/idlestat.rs), [sched/src/pcsample.rs](../../sched/src/pcsample.rs), [sched/src/gdwake.rs](../../sched/src/gdwake.rs) |
| Kernel facilities | [sched/src/irqctx.rs](../../sched/src/irqctx.rs), [sched/src/ksync.rs](../../sched/src/ksync.rs), [sched/src/kthread.rs](../../sched/src/kthread.rs), [sched/src/workqueue.rs](../../sched/src/workqueue.rs), [sched/src/hrtimer.rs](../../sched/src/hrtimer.rs), [kernel/src/ktest.rs](../../kernel/src/ktest.rs) |
| CPU bring-up | [arch/x86_64/src/smp.rs](../../arch/x86_64/src/smp.rs), [arch/x86_64/src/cpus.rs](../../arch/x86_64/src/cpus.rs), [arch/aarch64/src/smp.rs](../../arch/aarch64/src/smp.rs) |

Regression tests: **`smpwaketest`** (futex/eventfd ping-pong across CPUs, under spin and yield load, WFI wake, a pipe-EPOLLET poller herd — every subtest reports `lost=0`), **`racetest`** (raw port send/recv blocking races), **`idletest`** (the system genuinely idles rather than busy-polling), **`pthreadtest`**, **`killmt`** (`kill -9` of a multithreaded process with siblings spinning on other CPUs), **`jobtest`** (job-control stops), **`polltest`**'s `poll_timeout_wake_latency` (1000 × 10 ms polls; p90 ≤ 5 ms, p99 ≤ 20 ms, none early), and the `ktest=` self-tests. Every change is checked on both architectures and, for SMP work, under a non-default topology (`LEANDROS_SMP=6,sockets=2,cores=3` gives sparse APIC IDs). See [development/testing.md](../development/testing.md).

## See also

- [processes.md](processes.md) — task lifecycle, exit, reaping, the exit log
- [event-loop.md](event-loop.md) — the poll wait channel and targeted wakes
- [time.md](time.md) — the tick grid and the monotonic clock
- [invariants.md](invariants.md) — the rules the scheduler depends on
- [development/debugging.md](../development/debugging.md) — `[WDOG]`, `[TIMER]`, Ctrl-T
