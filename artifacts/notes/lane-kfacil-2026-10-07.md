# lane/kfacil — 2026-10-07 — kernel facilities for the OpenRM shim

Branch `lane/kfacil`, base main `b92798a`. Not pushed. Mac: aarch64 under HVF,
x86_64 under TCG.

This lane adds the generic kernel infrastructure that the OpenRM spike
(`lane-openrm-2026-10-07.md` §3, "Kernel prerequisites") found missing. The RM
shim lane builds its `os_*` / `nv_*` callbacks on top of it. Every facility has
a Rust API and, where the shim needs one, a thin `extern "C"` entry point
(`kfacil_*`). There is a boot self-test for each, behind `ktest=` on the kernel
command line.

## Execution contexts (read this first)

| context | how you get there | IRQs | may sleep |
|---|---|---|---|
| hard IRQ | any interrupt handler: tick, IPIs, `drivers::irq` handlers, IRQ-mode hrtimer callbacks | masked | no |
| task, atomic | holding a `KSpinLock` / `RawSpinLock`, a lockwatch-tracked lock (`RUN_QUEUE`, `VIRTIO_GPU`, `FD_TABLES`, ...), or inside `preempt_disable` | masked | no |
| task | syscall / ioctl, fault handler, kernel thread, work item, worker-mode hrtimer callback | masked (as every syscall) | yes |
| boot | `kernel_main` / `init_task_main` before `sched::run` (no current task) | either | no: waits spin |

- **`sched::irqctx`** keeps a per-CPU hard-IRQ nesting counter.
  - Maintained on both arches:
    - x86_64: the tick/resched path in `idt::fault_common`, every dynamic
      device vector, and the TLB-shootdown IPI.
    - aarch64: `exception::handle_irq`.
  - The counter is always dropped **before** the IRQ path may reschedule
    (`preempt_check`), so it never leaks into another task.
- **Sleeping where it is not allowed does not park.** The blocking calls
  (`KMutex::lock`, `KSemaphore::down*`, `KWaitQueue::wait_event*`,
  `KCompletion::wait*`, `KRwLock::read/write`, `kthread::msleep`) check
  `might_sleep()`. When sleeping is not allowed:
  - In atomic or IRQ context they log once per call site:
    `[KSYNC] sleep in atomic context (<reason>) at file:line; spinning instead`.
  - They then spin: pure spin in IRQ / KSpinLock context, `irq_window` with
    preemption held off otherwise.
  - In boot context they just spin, without logging.
  - So sleeping under a spinlock cannot happen, and it is reported once per site.
- **Limit:** plain `spin::Mutex` guards are invisible to the check (there is no
  hook in the `spin` crate). Locks that a sleeping path must not be holding
  must be a `KSpinLock` or a lockwatch `TrackedMutex`.
- **Kernel threads start with IRQs masked, like syscalls.** They are preempted
  only where they park, or in `kthread::cond_resched()`. A long compute loop
  must call `cond_resched()`, or the `[WDOG]` watchdog reports the CPU after 2 s.

## API summary

### 1. Sleeping synchronisation — `sched::ksync`

```rust
// IRQ-safe spinlocks (masks IRQs, raises atomic depth; usable in ISRs)
pub struct RawSpinLock;      // #[repr(C)], 4 bytes, const new()
  fn lock_irqsave(&self) -> IrqFlags; fn unlock_irqrestore(&self, IrqFlags);
  fn try_lock_irqsave(&self) -> Option<IrqFlags>;
pub struct KSpinLock<T>;     // lock() -> KSpinGuard, try_lock()
pub fn irq_save() -> IrqFlags; pub fn irq_restore(IrqFlags);

pub struct KRawMutex;        // const new(); FIFO hand-off; not recursive
  fn lock(&self); fn try_lock(&self) -> bool; fn lock_deadline(&self, deadline_ns) -> bool;
  fn unlock(&self); fn is_locked(&self) -> bool; fn is_owned_by_me(&self) -> bool;
pub struct KMutex<T>;        // lock() -> guard, try_lock(), lock_timeout(ns) -> Option<guard>

pub struct KRawRwLock;       // writer-preferring, hand-off to queue head
  fn read(&self); fn write(&self); fn try_read(&self) -> bool; fn try_write(&self) -> bool;
  fn read_unlock(&self); fn write_unlock(&self);
pub struct KRwLock<T>;       // read()/write()/try_read()/try_write() -> guards

pub struct KSemaphore;       // const new(count: i64)
  fn down(&self); fn down_timeout(&self, ns) -> bool; fn down_deadline(&self, abs_ns) -> bool;
  fn try_down(&self) -> bool; fn up(&self) /* ANY context, incl. IRQ */;

pub struct KWaitQueue;       // const new()
  fn wait_event(&self, cond: impl FnMut() -> bool);
  fn wait_event_timeout(&self, cond, ns) -> bool;   // final cond value
  fn wait_event_deadline(&self, cond, abs_ns) -> bool;
  fn wake_one(&self) -> bool; fn wake_all(&self) -> usize;  /* ANY context */

pub struct KCompletion;      // complete()/complete_all() any context; wait(), wait_timeout(ns) -> bool
pub fn deadline_after(timeout_ns: u64) -> u64;      // u64::MAX = forever
```

**Context table:**

| call | task | atomic | IRQ | boot |
|---|---|---|---|---|
| spinlocks, `try_*`, `done` | yes | yes | yes | yes |
| `up`, `wake_*`, `complete*` | yes | yes | **yes** | yes |
| `lock`/`down`/`wait*`/`read`/`write` | sleeps | logged, spins | logged, spins | spins |
| mutex/rwlock unlock | yes | yes | only after a `try_*` in that same handler | yes |

**Mechanism:**
- A waiter parks on a new sentinel channel, `sched::KWAIT_CHANNEL`
  (0xFFFF_FF03), and is woken **by pid** through `sched::kwake(pid)`. Each
  primitive keeps its own intrusive FIFO waiter list (the nodes live on the
  waiters' stacks).
- `kwake` only *try*-locks `RUN_QUEUE` (bounded 50 µs spin). On contention the
  wake is recorded, and the next tick on any CPU pays it. So every wake is
  IRQ-safe and deadlock-free.
- A timed wait records its deadline in the task. `wake_due_poll_deadlines`
  releases it exactly like a poll waiter, through the per-CPU one-shot timer.
- Waits are uninterruptible: a signal wakes the task, and the wait parks again.
- A woken waiter that finds its condition false re-links itself before it
  parks again (bug found and fixed in this lane; see "Bugs found").

**C ABI** (`sched::ksync::ffi`). These are opaque heap objects. `create` and
`destroy` run in task context.
- spinlock: `kfacil_spinlock_create/destroy`,
  `kfacil_spin_lock_irqsave(l) -> flags`, `kfacil_spin_unlock_irqrestore(l, flags)`
- mutex: `kfacil_mutex_create/destroy/lock/trylock/unlock/held_by_me`
- rwlock: `kfacil_rwlock_create/destroy/read/write/try_read/try_write/read_unlock/write_unlock`
- semaphore: `kfacil_sema_create(count)/destroy/down/down_timeout(s, ns)/try_down/up`
- wait queue: `kfacil_waitq_create/destroy`,
  `kfacil_waitq_wait(q, cond: extern "C" fn(*mut c_void) -> bool, arg, timeout_ns) -> bool`,
  `kfacil_waitq_wake_one/wake_all`
- context queries: `kfacil_in_irq()` (RM `os_is_isr`) and `kfacil_can_sleep()`
  (RM `os_semaphore_may_sleep`)

### 2. Context queries — `sched::irqctx`

```rust
pub fn in_irq() -> bool; pub fn in_interrupt() -> bool; pub fn irq_depth() -> u32;
pub fn can_sleep() -> bool; pub fn in_atomic() -> bool;
pub fn sleep_forbidden_reason() -> Option<&'static str>;
#[track_caller] pub fn might_sleep() -> bool;      // logs once per call site
pub fn irq_enter(); pub fn irq_exit();              // arch entry paths only
pub fn atomic_inc(); pub fn atomic_dec(); pub fn atomic_depth() -> u32;  // for custom spinlocks
pub fn sleep_violations() -> u32; pub fn irq_count(cpu) -> u32;
#[no_mangle] extern "C" fn sched_in_interrupt() -> bool;
```

### 3. Interrupts — `drivers::irq` (+ PCI MSI/MSI-X)

```rust
pub type IrqHandler = extern "C" fn(ctx: usize) -> IrqReturn;   // IrqReturn::{None, Handled}
pub enum IrqSource { Vector(u8) /* x86_64 */, Line(u32) /* aarch64 GIC INTID */ }
pub fn request_irq(src: IrqSource, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError>;
pub fn free_irq(h: IrqHandle);
pub fn alloc_vector() -> Option<u8>; pub fn free_vector(v: u8);       // x86_64, 0x41..=0xEF
pub fn request_fixed_vector(v: u8, h, ctx) -> bool;                   // legacy (keyboard IRQ1 = 33)
pub fn msi_message(vector: u8) -> Option<(u64 addr, u32 data)>;
pub fn request_msi(f: PciFunction, h, ctx) -> Result<IrqHandle, IrqError>;
pub fn msi_set_masked(h: &IrqHandle, masked: bool) -> bool;           // if the function supports it
pub struct MsixTable;  // MsixTable::enable(f) -> Result<MsixTable>
  fn request(&self, entry: u16, h, ctx) -> Result<IrqHandle>;
  fn set_masked(&self, entry, bool); fn pending(&self, entry) -> bool;
  fn set_function_masked(&self, bool); fn entries(&self) -> u16; fn disable(self);
pub fn vector_count(v) -> u64; pub fn line_count(id) -> u64;
```

**x86_64:**
- Every vector `0x21..=0xEF` except 0x40 (the reschedule IPI) enters one
  16-byte stub `push v; jmp irq_dyn_common` (`arch_x86_64::idt`).
- `irq_dyn_common` saves the caller-saved registers, does `swapgs` /
  `restore_user_gs` for an interrupt taken from ring 3, and calls
  `irq_dyn_dispatch(v)`. That sends the **EOI centrally, first**, then
  `irq_enter` → handler → `irq_exit`. There is no reschedule on this path,
  the same as the old handlers.
- The MSI message is the LAPIC window `0xFEE0_0000`, destination APIC id 0
  (BSP), fixed delivery, edge. It is the message virtio-gpu always wrote.
- Vectors are allocated from 0x41 upward. An unregistered vector is counted
  and logged once.

**Migrations, with no behaviour change:**
- virtio-gpu's control-queue MSI-X vector (hardwired 0x41) now comes from
  `alloc_vector()`, and its handler is registered before the entry is
  unmasked. It is still 0x41 whenever virtio-gpu is the first MSI user. In a
  ktest run where edu allocated first, the GPU got 0x41 after edu freed it.
  `MSIX_VECTOR_CTRLQ` is now an `AtomicU8`.
- The PS/2 keyboard's legacy vector 33 is now registered through
  `request_fixed_vector`.
- Timer (32), reschedule (0x40), TLB shootdown (0xFD) and spurious (0xFF) stay
  system vectors.
- **x86_64 INTx (`IrqSource::Line`) returns `Unsupported`.** There is no ACPI
  `_PRT` parser; use MSI on x86_64.

**aarch64:**
- `IrqSource::Line(intid)` registers in `drivers::irq`'s line table: 512
  lines, up to 4 sharers each.
- The line is enabled level-triggered through the new
  `arch_irq_line_enable/disable` hooks.
- The GIC dispatcher falls back to `drivers::irq::dispatch_line` for INTIDs it
  does not own.
- Storm guard: after 1000 deliveries in a row that no handler claims, the line
  is masked and logged.
- `PciFunction::intx_line()` gives QEMU virt's INTx swizzle (SPIs 3..6).
- **MSI on aarch64 is a TODO: the kernel has no GICv3 ITS code.**
  `request_msi` / `MsixTable::enable` return `IrqError::Unsupported`.

**Handler contract:**
- Hard-IRQ context, IRQs masked. No sleeping.
- Allowed: `KSpinLock`, `up` / `wake` / `complete`, `WorkQueue::queue`, and
  atomic-pool allocations (plain `Box` / `Vec` are routed there automatically).
- A level-triggered (INTx) handler must quiesce its device before returning.

### 4. Kernel threads and deferred work — `sched::kthread`, `sched::workqueue`

```rust
// kthread
pub fn spawn<F: FnOnce() + Send + 'static>(name: &str, f: F) -> Option<KThread>;
  KThread::{pid, join, join_timeout(ns) -> bool, request_stop, stop, finished}
pub fn should_stop() -> bool;                                // inside the thread
pub fn msleep(ms); pub fn sleep_ns(ns); pub fn sleep_until(abs_ns); pub fn udelay(us);
pub fn cond_resched(); pub fn yield_now();
#[no_mangle] extern "C" fn kfacil_kthread_spawn(func: extern "C" fn(usize), arg: usize) -> u32 /*pid, 0=fail*/;
// lower level: sched::spawn_with_arg(func: extern "C" fn(usize) -> !, arg) -> Option<Pid>

// workqueue
pub struct Work;  Work::new(func: extern "C" fn(usize), ctx) (const), set_ctx, is_pending
pub struct WorkQueue;  WorkQueue::new(name) -> Option<Arc<WorkQueue>>
  fn queue(&self, &'static Work) -> bool;         // ANY context, no allocation
  unsafe fn queue_raw(&self, *const Work) -> bool;
  fn queue_fn(&self, FnOnce()) -> bool;           // allocates (IRQ: atomic pool)
  fn cancel(&self, *const Work) -> bool; fn flush(&self); fn destroy(&self);
  fn worker_pid(&self) -> Pid; fn backlog(&self) -> u64;
pub fn init_system() -> bool; pub fn system() -> Option<&'static WorkQueue>;
pub fn queue_work(&'static Work) -> bool;          // system queue
#[no_mangle] kfacil_work_alloc(func, ctx) -> *mut Work, kfacil_work_free, kfacil_queue_work(w) -> bool, kfacil_flush_system_wq()
```

- **Arguments.** A kthread's argument travels in callee-saved registers: rbx/r12
  on x86_64, x19/x20 on aarch64. A tiny asm `kthread_trampoline` moves it into
  the first argument register.
- **Exit and naming.** When a thread's function returns, the thread calls
  `sched::exit(0)` and the scheduler reaps it. The ktest confirms the task
  count returns to its baseline. The thread shows in Ctrl-T dumps as
  `[name]` (through its exe path).
- **System queue.** It is started in `init_task_main` as the `[kworker]` task,
  one extra kernel task on every boot.
- **One worker per queue.** Each queue runs its items FIFO, one at a time, in
  task context, and the items may sleep.
- **Re-queueing.** Queueing a pending item returns false (Linux semantics).
- **`flush` from the queue's own worker** is detected, logged once, and ignored.
  Otherwise it would deadlock.
- **RM mapping.**
  - `rm_isr_bh` / `os_queue_work_item` → `queue_raw` of a `Work` that RM's
    shim allocates.
  - `os_flush_work_queue` → `flush`.

### 5. Kernel VA for device and scattered mappings — `mm::kva`

```rust
pub enum CacheMode { Uncached, WriteCombining, WriteBack }
pub fn ioremap(phys: u64, len: usize, mode: CacheMode) -> Option<usize>;   // VA of `phys` (page offset kept)
pub fn iounmap(va: usize) -> bool;                                         // any VA inside the mapping
pub fn vmap(pages: &[u64], mode: CacheMode) -> Option<usize>;              // scattered 4 KiB pages -> contiguous VA
pub fn vunmap(va: usize) -> bool;
pub fn in_window(va) -> bool; pub fn stats() -> (live, pages, cursor);
#[no_mangle] kfacil_ioremap(phys, len, mode 0/1/2) -> *mut u8, kfacil_iounmap(va) -> bool, kfacil_vmap(pages, n, mode) -> *mut u8
```

- **The window.** `0xffff_f000_0000_0000`, 256 GiB: top-level slot 480 on both
  arches, outside the HHDM, so no HHDM huge page is ever split. Pages are 4 KiB
  PTEs.
- **x86_64 detail.** User PML4s copy the kernel half when they are created, so
  `kfacil_init` (in `kernel_main`, before any user address space exists)
  creates the window's top-level entry with a probe map/unmap.
- **Cache modes.**
  - x86_64: UC = PCD (UC-; UC under the MMIO MTRR); WC = PAT entry 5 (the
    existing `init_pat_bsp` setup); WB = plain.
  - aarch64: UC = Device-nGnRnE; WC = Normal Non-cacheable; WB = Normal WB.
- **Allocation.** Next-fit, with a one-page guard after each mapping. Freed
  ranges are reused only after the cursor wraps, so a CPU that missed a
  bounded x86 shootdown ack can never see a recycled range.
- **Unmap.** `iounmap` flushes every CPU: a CR3-reload broadcast on x86_64,
  `tlbi vaae1is` on aarch64.
- **Limits.**
  - Task context only.
  - No huge pages: a 256 MiB BAR costs 512 KiB of page tables. Mapping 64 MiB
    took about 0–10 ms in QEMU.
  - Do not map RAM with a different cache type from its HHDM alias on real
    hardware, unless you know the platform tolerates it. RM's
    `nv_alloc_kernel_mapping` of system pages should use WB.

### 6. IRQ-safe allocation — `mm::atomic_pool`

```rust
pub fn alloc(size: usize, align: usize) -> Option<*mut u8>;  // never blocks
pub fn free(ptr: *mut u8); pub fn owns(ptr) -> bool;
pub fn census(emit); pub fn failures() -> u32;
#[no_mangle] kfacil_kmalloc(size, atomic: u32) -> *mut u8;   // 16-B aligned, size header
#[no_mangle] kfacil_kfree(ptr);                               // any context
```

- **The pool.** 256 KiB carved at boot into classes:
  32×512, 64×512, 128×256, 256×128, 512×64, 1K×32, 2K×16, 4K×12. Objects are
  naturally aligned. The pool's lock is only taken with IRQs masked, so it
  never blocks and never calls into the buddy allocator after boot.
- **Global allocator.** In hard-IRQ context it now tries the pool first, then
  falls back to the old path, so behaviour can only improve. `dealloc` returns
  pool pointers to the pool from any context.
- **`kfacil_kmalloc(…, atomic=1)`** tries the pool, then a slab `try_lock`
  (never spins).
- **Limits.**
  - At most 4 KiB from the pool.
  - About 216 KiB in total, with no growth.
  - An exhausted class is logged once:
    `[ATOMIC-POOL] a size class ran out (IRQ-context allocation pressure)`.
  - Larger ISR buffers must be preallocated in task context.
  - This matches Haiku's IntrSafePool use for `os_alloc_mem` from `rm_isr`.

### 7. High-resolution timers — `sched::hrtimer`

```rust
pub enum TimerMode { Irq, Worker }
HrTimer::new(func: extern "C" fn(usize), ctx, mode) -> Box<HrTimer>
  fn start(&self, delay_ns, period_ns /*0 = one-shot*/) -> bool;
  fn start_at(&self, abs_ns, period_ns) -> bool;
  fn cancel(&self) -> bool; fn cancel_sync(&self) -> bool;   // Drop = cancel_sync
  fn is_armed, fired, expires
#[no_mangle] kfacil_hrtimer_create(func, ctx, mode 0=IRQ/1=worker) / destroy / start(t, delay, period) / cancel
```

- **Expiry.** Driven by the per-CPU one-shot timer the poll-deadline code
  already arms (`sched::timer_deadline_irq` services hrtimers first), with
  every CPU's 100 Hz tick as a fallback.
- **Callback context.**
  - `Irq` callbacks run in hard-IRQ context.
  - `Worker` callbacks run on the system work queue (task context, may sleep).
- **Periodic timers.** They skip missed periods and never run their callback
  concurrently with itself.
- **Capacity.** 128 armed timers system-wide.
- **Measured lateness of a one-shot:** 15–220 µs on aarch64/HVF, 90–2500 µs on
  x86_64/TCG.

### 8. PCI helpers — `drivers::pci`

```rust
pub struct PciFunction { bus, dev, func }      // PciDevice::function(), find_function(vid, did)
  fn read8/16/32(&self, off: u16); fn write8/16/32(&self, off: u16, v);   // full 4 KiB
  fn ecam_page(&self) -> usize; fn has_extended_config(&self) -> bool;
  fn capabilities(&self) -> Vec<(u8, u16)>; fn find_capability(&self, id: u8) -> Option<u16>;
  fn ext_capabilities(&self) -> Vec<(u16 id, u8 ver, u16 off)>; fn find_ext_capability(&self, id: u16) -> Option<u16>;
  fn bar(&self, i) -> Option<BarInfo>;           // decode only
  fn size_bars(&self) -> BarSizing;              // safe sizing (factored out of nvidia_probe)
  fn map_bar(&self, i, CacheMode) -> Option<(va, size)>;
  fn enable_memory, enable_io, set_bus_master(bool), enable_mem_and_bus_master, set_intx_disable(bool), command, set_command
  fn intx_line(&self) -> Option<u32>;            // aarch64 QEMU virt only
pub fn init_ecam_from_acpi(rsdp_phys, hhdm);    // x86_64, called in kfacil_init
pub unsafe fn pci_write_config_8(bus, dev, func, off, v);
pub mod cap { PM, MSI, VENDOR, PCIE, MSIX, EXT_AER, EXT_REBAR }
```

- **Accesses.** Each access is a single sized one: no read-modify-write of
  neighbouring bytes, so RW1C status bits survive.
- **x86_64.**
  - Offsets below 0x100 use legacy 0xCF8. Those accesses are now serialised by
    an IRQ-safe lock; the old helpers were not.
  - Offsets from 0x100 up use ECAM from ACPI MCFG. A function's page is mapped
    (UC) on first use, which must happen in task context; in IRQ context an
    unmapped page reads as all-ones.
- **aarch64.** ECAM always (the existing window).
- **nvprobe.** `nvidia_probe::size_bars` is now a thin adapter over
  `PciFunction::size_bars`. The register sequence is identical; its own
  `irq_save/restore` became `arch_interrupt_save/restore`, which does the same
  thing. `nvprobe.vendor=0x1af4` output was compared against main; see the
  evidence section.

### 9. `request_firmware` — `drivers::firmware` (+ backend `kernel/src/firmware.rs`)

```rust
pub fn root_ready() -> bool;                                  // real root mounted at "/"
pub fn firmware_size(path: &str) -> Result<u64, FwError>;
pub fn read_firmware_chunked(path, sink: &mut dyn FnMut(u64 off, &[u8])) -> Result<u64, FwError>;
pub fn request_firmware(path: &str, max_len: usize) -> Result<Firmware, FwError>;
  Firmware::{len, pages() -> &[u64] /*phys, file order*/, read_at(off, buf), chunks(), map() -> Option<va>, to_vec()}
  // Drop frees the pages and the mapping
pub enum FwError { NoRoot, Fs(i32 errno), NoMemory, TooBig }
#[no_mangle] kfacil_request_firmware(path: *const u8, len_out: *mut usize, data_out: *mut *const u8) -> *mut Firmware;
#[no_mangle] kfacil_release_firmware(fw);
```

- **No large contiguous allocation.** The file goes into a page list built
  from 64 KiB buddy blocks, falling back to 4 KiB.
- **`map()`** vmaps it contiguously (WB). That is the pointer RM's
  `nv_get_firmware` returns; `kfacil_request_firmware` does this for the
  caller.
- **Backend.** It goes straight to the f2fs mount that `pivot_root` made "/".
  It does not use the fd table or user memory, and it does not resolve the
  caller's cwd/chroot.
  - Open: `vfs::call_port(port, VFS_OPEN)`.
  - Size: `VFS_LSEEK(SEEK_END)`.
  - Read: `f2fs_server::pread_by_port` into a 1 MiB bounce buffer, holding
    `F2FS_MOUNTS` per chunk only, with `cond_resched` between chunks.
  - Close: `close_by_port`.
  - Credentials: root in a kthread; the caller's own in an ioctl.
- **Before userspace init's `pivot_root`, every call returns `NoRoot`.** The
  RM shim must not ask for the GSP image before that. Typically `rm_init_adapter`
  runs from a kthread that waits on `root_ready()`, or from the first open.
- **Context.** Task context only. Not from IRQ handlers, and not while holding
  a spinlock.
- **Why the backend lives in the kernel crate.** `drivers` cannot depend on the
  VFS or f2fs (that would be a cycle), so the kernel registers the backend as
  a vtable at init.

## Suggested RM callback mapping (for the shim lane)

| RM callback | facility |
|---|---|
| `os_alloc_spinlock` / `os_acquire_spinlock` / `os_release_spinlock` | `kfacil_spinlock_create`, `kfacil_spin_lock_irqsave` (returns the flags RM hands back), `kfacil_spin_unlock_irqrestore` |
| `os_alloc_mutex` / `os_acquire_mutex` / `os_cond_acquire_mutex` / `os_release_mutex` | `kfacil_mutex_*`; the cond variant is `trylock` |
| `os_alloc_rwlock` / `os_acquire_rwlock_read/write` / `os_cond_*` / `os_release_rwlock_*` | `kfacil_rwlock_*` |
| `os_alloc_semaphore` / `os_acquire_semaphore` / `os_cond_acquire_semaphore` / `os_release_semaphore` | `kfacil_sema_*`; release works in an ISR |
| `os_alloc_wait_queue` / `os_wait_uninterruptible` / `os_wait_interruptible` / `os_wake_up` | `kfacil_waitq_*` with a flag condition (set the flag, then `wake_all`) |
| `os_is_isr` / `os_semaphore_may_sleep` | `kfacil_in_irq` / `kfacil_can_sleep` |
| `os_alloc_mem` / `os_free_mem` | `kfacil_kmalloc(size, kfacil_in_irq())` / `kfacil_kfree` |
| `os_queue_work_item` / `os_flush_work_queue` | `kfacil_work_alloc` + `kfacil_queue_work` / `kfacil_flush_system_wq`, or a private `WorkQueue` (RM bottom half) |
| `os_map_kernel_space` / `os_unmap_kernel_space`, `nv_alloc_kernel_mapping` | `kfacil_ioremap(phys, len, mode)` / `kfacil_iounmap`; `kfacil_vmap(page_list, n, 2 = WB)` |
| `os_pci_read/write_byte/word/dword` | `PciFunction::read8/16/32` and `write8/16/32` (full 4 KiB) |
| MSI + ISR install | `drivers::irq::request_msi(func, isr, nv_ptr)`; the ISR calls `rm_isr` and queues the bottom half |
| `nv_get_firmware` / `nv_put_firmware` | `kfacil_request_firmware(path, &len, &data)` / `kfacil_release_firmware` (call only after `root_ready()`) |
| `os_delay` / `os_delay_us` | `kthread::msleep` (parks) / `kthread::udelay` (spins) |
| RC / nano timers (`tmrCtrl` callbacks) | `kfacil_hrtimer_*` (`mode = 1` if the callback takes locks that sleep) |

## Self-tests

Kernel command line (Limine `cmdline:`):
`ktest=all` or a comma list of
`irqctx,sync,kthread,workqueue,hrtimer,atomicpool,kva,pci,msi,firmware`.
`ktest.fw=/path` picks the firmware test's file; by default it uses the largest
of `/bin/mame` and a few others.

The tests run in the `[ktest]` kernel thread and log through the raw UART:
- one `[KTEST] <name>: ok|FAIL <check>` line per check;
- `[KTEST] <name> PASS|FAIL|SKIP` per test;
- `[KTEST] summary: N pass, M fail, K skip; sleep-in-atomic detections D`.

The irqctx test deliberately triggers one detection, so `D=1` is expected.

QEMU devices for the device tests:

```
LEANDROS_QEMU_EXTRA="-device edu,dma_mask=0xffffffffffffffff -device virtio-rng-pci,disable-legacy=on -device pcie-root-port,id=ktrp,chassis=7"
```

- **`edu`** (QEMU's teaching device) covers the MSI test on x86_64 and the
  INTx test on aarch64: interrupt-raise register, DMA RAM→device→RAM with a
  completion interrupt.
  - `dma_mask` is needed on aarch64, where RAM starts at 1 GiB; edu masks to
    28 bits by default.
- **The second `virtio-rng-pci`** is the MSI-X test device. The kernel's rng
  driver claims the first one. The test drives queue 0 itself and checks:
  - the completion interrupt arrives on MSI-X entry 0;
  - with the entry masked there is no delivery and the PBA pending bit is set;
  - unmasking delivers the pending interrupt.
- **`pcie-root-port`** gives the extended-capability walker something to walk
  (AER at 0x100, ACS at 0x148).

How to build a test image from the main tree's images (APFS clones; only the
kernel and `limine.conf` change):

```sh
cp -c ../../leandros-limine-$ARCH.img .; mcopy -o -i leandros-limine-$ARCH.img@@1048576 target/final-$ARCH/kernel ::/kernel.elf
# limine.conf: add "\tcmdline: ktest=all" after the "path:" line, mcopy it to ::/limine.conf
```

## Evidence (final tree)

| | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| `ktest=all` (devices above) | **10/10 PASS** | **10/10 PASS** |
| irqctx | in_irq/depth/!can_sleep inside an IRQ hrtimer callback; KSpinLock + preempt_disable detected | same |
| sync | mutex 4×200 = 800 with sleeps inside; lock_timeout 30 ms; rwlock 3 readers overlap, writer exclusive; `up()` from IRQ releases `down()` after 31 ms; wait queue `wake_all` from IRQ wakes 3; completion | same |
| kthread | 16 closures (sum 136), C fn+ctx spawn, stop/join, task count back to baseline, msleep(25) = 25.4 ms | same |
| workqueue | 100 closures FIFO + flush; queue from IRQ runs in worker (task ctx, sleeps); pending refusal; cancel; private queue destroy | same |
| hrtimer | one-shot lateness 15–220 µs; periodic 10 ms × 205 ms = 20; cancel; worker mode | lateness 90–2500 µs; periodic 20 |
| atomicpool | IRQ-context `Box`/`Vec` from pool, freed back; 4 KiB class exhausts at 12; kmalloc fallback | same |
| kva | WB alias, write-through; vmap of 8 scattered pages; 64 MiB mapping; next-fit; no leaks | same |
| pci | `PciFunction` == legacy on every function; caps; `size_bars` restores; write8 single byte; ECAM; root port ext caps AER@0x100, ACS@0x148 | same + MCFG ECAM |
| msi | edu **INTx** on GIC INTID 37: raise + DMA round trip, 3 IRQs, none after `free_irq`; MSI → `Unsupported` | edu **MSI** (vector 0x41/0x42) + DMA; virtio-rng **MSI-X** deliver / mask+PBA pending / unmask; vector allocator |
| firmware | `/bin/mame` 390,174,912 B; streamed == page list == vmap view; cksum **3351706609**, which matches guest `cksum` | `/bin/mame` 396,812,352 B, cksum **211130145**, matches guest `cksum` |
| default boot (no cmdline) | login, unchanged | login, `[GPU] MSI-X armed: control queue -> vector 0x41` |
| `runtests regress` (default image, fresh data disks) | **15/15 PASS** | **15/15 PASS** |
| desktop smoke `ffsession.py --nofirefox` (virgl) | greeter 0.7 / desktop 0.7 / term 0.7 s; virtio-gpu INTx unchanged | greeter 1.3 / desktop 14.5 / term 1.3 s; `[GPU] MSI-X armed: control queue -> vector 0x41`, GPU interrupts counted on the dynamic vector |
| nvprobe A/B (`nvprobe.vendor=0x1af4`, old probe file vs factored) | `[NVPROBE]` output identical (18 lines) | identical (19 lines) |

## Bugs found on the way

- **Lost wake in the wait queue.** Found by the x86 workqueue test: `flush`
  hung. A waker could pop a waiter's node between the waiter linking it and
  publishing Blocked. Its `kwake` then found the task still running, and the
  waiter parked unlinked, where no later wake could find it. Fixed: after
  publishing Blocked, the waiter re-checks under the queue lock that it is
  still linked, and loops if not. The hand-off primitives were never
  affected, because they test `granted` after publishing Blocked.
- **Self-deadlock on `VIRTIO_GPU`, pre-existing and latent, NOT fixed here.**
  1. Any kernel thread printing through the framebuffer console
     (`serial_print_str`) while another CPU initialises virtio-gpu
     (`gpuprobe`'s ioctl holds `VIRTIO_GPU`) queues lines in the console
     outbox.
  2. That CPU's own `[GPU]` diagnostics go through `arch_serial_putc`, which
     drains the outbox.
  3. The drain writes the queued lines to the framebuffer and flushes it,
     which takes `VIRTIO_GPU` again: `[WDOG] ... spinning for VIRTIO_GPU held
     by cpu1`.

  The ktest now logs through the raw UART only. The proper fix belongs to the
  console code: an `arch_serial_putc` drain should not flush the framebuffer.
- **`relax()` spin fallback could reschedule while a tracked lock is held.**
  Inherent in `irq_window` → tick → `preempt_check`. The non-sleeping
  fallbacks now wrap their `irq_window` in `preempt_disable` /
  `preempt_enable_no_resched`.

## Left for later / known limits

- **aarch64 MSI / MSI-X** needs a GICv3 ITS driver. The API is in place and
  returns `Unsupported`.
- **x86_64 legacy INTx routing** needs ACPI `_PRT`.
- **MSI targets the BSP only** (APIC id 0). There is no affinity, and
  multi-vector MSI is not supported (one vector per function with MSI; MSI-X
  has per-entry vectors).
- **No huge pages in the KVA window.** Mapping all of an 8 GiB ReBAR BAR1 would
  cost 16 MiB of page tables; map sub-ranges as RM does.
- **The atomic pool does not grow.**
- **`spin::Mutex` holders are invisible to `might_sleep`.**
- **hrtimer capacity is 128 armed timers, and `start_at` from a callback
  re-arms on the CPU that serviced it.**
- **`request_firmware` needs the real root.** The initrd and RAMFS paths are
  not searched.
- **`kfacil_*` C symbols are not referenced by the default kernel**; fat LTO
  may drop them until the nvidia-rm crate links against them.
- **Latent console/virtio-gpu deadlock**: see "Bugs found".
