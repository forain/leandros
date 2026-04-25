//! Cooperative scheduler — context switching, task lifecycle, IPC blocking.
//!
//! Design: single-CPU, cooperative.  Tasks run until they call `yield_now()`,
//! `block_on()`, or `exit()`.  A static idle context in `run()` is the
//! "scheduler thread" that picks the next ready task on each wake-up.
//!
//! Analogues: Linux kernel/sched/core.c (`schedule`, `switch_to`).

#![no_std]

extern crate alloc;

pub mod clone;
pub mod context;
pub mod futex;
pub mod runqueue;
pub mod signal;
pub mod task;

pub use clone::{fork_current, clone_thread};
pub use signal::{check_and_deliver_signals, restore_signal_frame, sys_sigaction, sys_sigprocmask};
pub use futex::{futex_wait, futex_wake};

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use spin::Mutex;
use task::{Pid, Task, TaskState};
use context::CpuContext;
use runqueue::RunQueue;
use alloc::boxed::Box;

static RUN_QUEUE:       Mutex<RunQueue> = Mutex::new(RunQueue::new());
static NEXT_PID:        Mutex<Pid>      = Mutex::new(1);
static TIMER_TICKS:     AtomicU64       = AtomicU64::new(0);
/// Set by timer_tick_irq; cleared and acted on by preempt_check.
static PREEMPT_NEEDED:  AtomicBool      = AtomicBool::new(false);
/// Optional hook called with a PID just before its task slot is reclaimed.
/// Registered by the IPC layer to release ports owned by the exiting task.
static TASK_EXIT_HOOK:  AtomicPtr<()>   = AtomicPtr::new(core::ptr::null_mut());

// ── Exit-code log ────────────────────────────────────────────────────────────

const EXIT_LOG_LEN: usize = 256;

#[derive(Clone, Copy)]
struct ExitRecord { pid: Pid, code: i32 }
static EXIT_LOG: Mutex<[Option<ExitRecord>; EXIT_LOG_LEN]> = Mutex::new([const { None }; EXIT_LOG_LEN]);
static EXIT_LOG_IDX: Mutex<usize> = Mutex::new(0);

fn log_exit(pid: Pid, code: i32) {
    let mut log = EXIT_LOG.lock();
    let mut idx = EXIT_LOG_IDX.lock();
    log[*idx] = Some(ExitRecord { pid, code });
    *idx = (*idx + 1) % EXIT_LOG_LEN;
}

pub fn take_exit_code(pid: Pid) -> Option<i32> {
    let mut log = EXIT_LOG.lock();
    for entry in log.iter_mut() {
        if let Some(record) = entry {
            if record.pid == pid {
                let code = record.code;
                *entry = None;
                return Some(code);
            }
        }
    }
    None
}

// ── Per-CPU state ────────────────────────────────────────────────────────────

pub const MAX_CPUS: usize = 16;
static mut SCHEDULER_CTX: [CpuContext; MAX_CPUS] = [const { CpuContext::zeroed() }; MAX_CPUS];
static mut CURRENT_CTX:   [*mut CpuContext; MAX_CPUS] = [core::ptr::null_mut(); MAX_CPUS];
static mut CURRENT_PID:   [Pid; MAX_CPUS] = [0; MAX_CPUS];

extern "C" {
    pub fn cpu_id() -> usize;
    fn arch_set_kernel_stack(addr: u64);
    fn arch_set_page_table(addr: usize);
    pub fn arch_alloc_page_table_root() -> usize;
}

pub fn alloc_pid() -> Pid {
    let mut pid_guard = NEXT_PID.lock();
    let pid = *pid_guard;
    *pid_guard += 1;
    pid
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn init() {
    // Initialise the boot CPU's run queue with nothing (idle).
    // The kernel will call spawn() to add the first task.
}

pub fn timer_tick_irq() {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    PREEMPT_NEEDED.store(true, Ordering::Relaxed);
}

pub fn preempt_check() {
    if PREEMPT_NEEDED.swap(false, Ordering::Relaxed) {
        yield_now();
    }
}

pub fn handle_page_fault(addr: usize) -> bool {
    let pid = current_pid();
    if pid == 0 { return false; }
    
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if let Some(ref mut as_) = t.address_space {
            return as_.handle_user_page_fault(addr);
        }
    }
    false
}

pub fn ap_entry() -> ! {
    scheduler_run_loop()
}

pub fn unblock_port(port: u32) {
    RUN_QUEUE.lock().unblock_port(port);
}

pub fn spawn(entry: fn() -> !, _flags: usize) -> Option<Pid> {
    let mut pid_guard = NEXT_PID.lock();
    let pid = *pid_guard;
    *pid_guard += 1;

    // Allocate stack for the kernel task
    let stack_base = mm::buddy::alloc(1)?; // 2 pages = 8KB
    let stack_size = mm::buddy::PAGE_SIZE * 2;

    let task = Task::new_kernel(pid, entry as usize, stack_base, stack_size, 0);
    let mut rq = RUN_QUEUE.lock();
    if rq.enqueue(task) {
        Some(pid)
    } else {
        mm::buddy::free(stack_base, 1);
        None
    }
}

pub fn spawn_user_with_address_space(entry_point: usize, sp: usize, as_: mm::vmm::AddressSpace) -> Option<Pid> {
    let mut pid_guard = NEXT_PID.lock();
    let pid = *pid_guard;
    *pid_guard += 1;

    let stack_phys = mm::buddy::alloc(1)?; // 8KB kernel stack for syscalls/interrupts
    // Pass the HHDM virtual address as the stack base so that ctx.rsp points to
    // an address accessible after the user PML4 is loaded (HHDM is always present).
    let stack_virt = mm::phys_to_virt(stack_phys);
    let stack_size = mm::buddy::PAGE_SIZE * 2;
    let page_table = as_.page_table_root;

    let mut task = Task::new_userspace(pid, entry_point, sp, stack_virt, stack_size, page_table);
    // Store physical address for buddy::free on task exit.
    task.kernel_stack = stack_phys;
    task.address_space = Some(as_);
    
    let mut rq = RUN_QUEUE.lock();
    if rq.enqueue(task) {
        Some(pid)
    } else {
        mm::buddy::free(stack_phys, 1);
        None
    }
}

pub fn run() -> ! {
    scheduler_run_loop()
}

fn scheduler_run_loop() -> ! {
    let id = unsafe { cpu_id() };
    loop {
        let maybe_idx = { RUN_QUEUE.lock().pick_next() };

        if let Some(idx) = maybe_idx {
            let (ctx_ptr, pid, kernel_stack_top_virt, page_table) = {
                let rq = RUN_QUEUE.lock();
                let t = rq.get(idx).unwrap();
                // RSP0 must be a virtual (HHDM) address accessible in any PML4.
                // kernel_stack stores the physical page base for both kernel and user tasks;
                // phys_to_virt converts it to the HHDM virtual used as the kernel stack.
                let kst = mm::phys_to_virt(t.kernel_stack) + mm::buddy::PAGE_SIZE * 2;
                (&t.ctx as *const CpuContext, t.pid, kst, t.page_table)
            };

            unsafe {
                CURRENT_CTX[id] = ctx_ptr as *mut CpuContext;
                CURRENT_PID[id] = pid;
                
                // Mark task as Running before switching to it
                {
                    let mut rq = RUN_QUEUE.lock();
                    if let Some(t) = rq.get_mut(idx) {
                        t.state = TaskState::Running;
                    }
                }

                arch_set_kernel_stack(kernel_stack_top_virt as u64);

                if page_table != 0 {
                    arch_set_page_table(page_table);
                }

                context::cpu_switch_to(
                    core::ptr::addr_of_mut!(SCHEDULER_CTX[id]),
                    ctx_ptr,
                );

                arch_set_page_table(0);
                CURRENT_CTX[id] = core::ptr::null_mut();
                CURRENT_PID[id] = 0;

                // Task returned to scheduler. If it's still "Running", 
                // it means it yielded or was preempted, so set it back to Ready.
                {
                    let mut rq = RUN_QUEUE.lock();
                    if let Some(t) = rq.get_mut(idx) {
                        if t.state == TaskState::Running {
                            t.state = TaskState::Ready;
                        }
                    }
                }
            }

            let zombie_info = {
                let mut rq = RUN_QUEUE.lock();
                if let Some(t) = rq.get_mut(idx) {
                    if t.state == TaskState::Zombie {
                        Some((t.kernel_stack, t.pid, t.exit_code))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            if let Some((stack_base, zombie_pid, exit_code)) = zombie_info {
                let hook_ptr = TASK_EXIT_HOOK.load(Ordering::Acquire);
                if !hook_ptr.is_null() {
                    let hook: fn(u32) = unsafe { core::mem::transmute(hook_ptr) };
                    hook(zombie_pid);
                }
                log_exit(zombie_pid, exit_code);
                { RUN_QUEUE.lock().remove(idx); }
                mm::buddy::free(stack_base, 1);
            }
        } else {
            core::hint::spin_loop();
        }
    }
}

pub fn yield_now() {
    unsafe {
        let id  = cpu_id();
        let ctx = CURRENT_CTX[id];
        if !ctx.is_null() {
            context::cpu_switch_to(ctx, core::ptr::addr_of_mut!(SCHEDULER_CTX[id]));
        }
    }
}

pub fn exit(code: i32) -> ! {
    let id = unsafe { cpu_id() };
    let pid = unsafe { CURRENT_PID[id] };
    {
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            t.state = TaskState::Zombie;
            t.exit_code = code;
        }
    }
    yield_now();
    loop { core::hint::spin_loop(); }
}

pub fn wait_pid(pid: Pid) -> Option<i32> {
    if let Some(code) = take_exit_code(pid) {
        return Some(code);
    }
    None
}

pub fn current_pid() -> Pid {
    unsafe { CURRENT_PID[cpu_id()] }
}

pub fn current_ppid() -> Pid {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.ppid).unwrap_or(0)
}

pub fn current_pgid() -> Pid {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.pgid).unwrap_or(0)
}

pub fn ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

pub fn deliver_signal(pid: Pid, signo: u32) -> isize {
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.signal_pending |= 1u64 << (signo - 1);
        0
    } else {
        -3 // ESRCH
    }
}

pub fn current_cwd(buf: *mut u8, len: usize) -> isize {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid(pid) {
        let n = t.cwd_len.min(len);
        unsafe {
            core::ptr::copy_nonoverlapping(t.cwd.as_ptr(), buf, n);
        }
        n as isize
    } else {
        -1
    }
}

pub fn set_cwd(path: &[u8]) -> bool {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        let n = path.len().min(32);
        t.cwd[..n].copy_from_slice(&path[..n]);
        t.cwd_len = n;
        true
    } else {
        false
    }
}

pub fn set_pgid(pid: Pid, pgid: Pid) -> bool {
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.pgid = pgid;
        true
    } else {
        false
    }
}

pub fn setsid() -> Pid {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.sid = pid;
        t.pgid = pid;
        pid
    } else {
        0
    }
}

pub fn umask(mask: u32) -> u32 {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        let old = t.umask;
        if mask != u32::MAX {
            t.umask = mask & 0o777;
        }
        old
    } else {
        0
    }
}

pub fn heap_end() -> usize {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    rq.find_pid(pid).map(|t| t.heap_end).unwrap_or(0)
}

pub fn current_sid() -> Pid {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    rq.find_pid(pid).map(|t| t.sid).unwrap_or(0)
}

pub fn pending_signals() -> u64 {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    rq.find_pid(pid).map(|t| t.signal_pending).unwrap_or(0)
}

pub fn clear_pending_signal(signo: u32) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.signal_pending &= !(1u64 << (signo - 1));
    }
}

pub fn replace_signal_mask(new_mask: u64) -> u64 {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        let old = t.signal_mask;
        t.signal_mask = new_mask;
        old
    } else {
        0
    }
}

pub fn current_reply_port() -> u32 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.reply_port).unwrap_or(u32::MAX)
}

pub fn set_current_reply_port(port: u32) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.reply_port = port;
    }
}

pub fn block_on(port: u32) {
    let pid = current_pid();
    RUN_QUEUE.lock().block_on_port(pid, port);
    yield_now();
}

pub fn set_clear_child_tid(tidptr: usize) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.clear_child_tid = tidptr;
    }
}

pub fn set_fs_base(addr: u64) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.tls_base = addr;
        #[cfg(target_arch = "x86_64")]
        { t.ctx.fs_base = addr; }
        #[cfg(target_arch = "aarch64")]
        { t.ctx.tpidr_el0 = addr; }
    }
}

pub fn get_fs_base() -> u64 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.tls_base).unwrap_or(0)
}

pub fn replace_address_space(
    new_as: mm::vmm::AddressSpace,
    pt_root: usize,
    heap_start: usize,
    entry: usize,
    user_sp: usize
) -> ! {
    let pid = current_pid();
    {
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            t.address_space = Some(new_as);
            t.page_table    = pt_root;
            t.heap_start    = heap_start;
            t.heap_end      = heap_start;
        }
    }

    extern "C" {
        fn arch_execve_return(entry: usize, user_sp: usize) -> !;
    }

    unsafe {
        // Switch to the new address space before entering userspace.
        arch_set_page_table(pt_root);
        arch_execve_return(entry, user_sp);
    }
}

pub fn spawn_user(_entry_va: usize, _stack_va: usize, _priority: i8) -> Option<Pid> {
    None 
}

pub fn with_current_address_space<F, R>(f: F) -> Option<R>
where F: FnOnce(&mm::vmm::AddressSpace) -> R {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    let task = rq.find_pid(pid)?;
    match task.address_space {
        Some(ref as_) => Some(f(as_)),
        None => None,
    }
}

pub fn with_current_address_space_mut<F, R>(f: F) -> Option<R>
where F: FnOnce(&mut mm::vmm::AddressSpace) -> R {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    let task = rq.find_pid_mut(pid)?;
    match task.address_space {
        Some(ref mut as_) => Some(f(as_)),
        None => None,
    }
}

pub fn register_task_exit_hook(hook: fn(u32)) {
    TASK_EXIT_HOOK.store(hook as *mut (), Ordering::Release);
}
