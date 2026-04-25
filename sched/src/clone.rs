//! Process cloning — `fork()` and related primitives.
//!
//! # AArch64 fork
//!
//! `fork_current(frame_ptr)` takes a pointer to the `UserFrame` that the EL0
//! synchronous exception handler saved on the *parent's* kernel stack before
//! calling `syscall_dispatch`.  The frame contains the complete user-register
//! state at the moment of the `svc #0` instruction.
//!
//! The child task is given its own kernel stack with an identical `UserFrame`
//! copied to the top.  Its `CpuContext` has `lr = ret_to_user_fork`, so the
//! first time the scheduler picks the child it restores all user registers from
//! the frame and `eret`s into user space with `x0 = 0` (fork returns 0 in the
//! child).
//!
//! # x86-64
//!
//! The x86-64 SYSCALL path uses a per-CPU kernel stack (not the task's own
//! kernel stack), so no `UserFrame` is available.  `fork_current` returns
//! `ENOSYS` on x86-64; full x86-64 fork support requires switching to an
//! IST-style kernel stack per task and is deferred to Phase 1.5.

use crate::task::{self, DEFAULT_SIGACTION};

/// Perform a POSIX `fork()`.
///
/// `frame_ptr` — virtual address of the `UserFrame` saved on the parent's
/// kernel stack by the EL0 exception entry stub (AArch64 only; 0 on x86-64).
///
/// Returns the child PID (> 0) to the parent, or a negative `errno` on error:
/// * `-12` ENOMEM  — OOM or run queue full
/// * `-38` ENOSYS  — called on x86-64 (not yet implemented)
pub fn fork_current(frame_ptr: usize) -> isize {
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = frame_ptr;
        return -38; // ENOSYS on non-AArch64
    }

    #[cfg(target_arch = "aarch64")]
    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
        if parent_pid == 0 { return -38; }

        // ── Step 1: allocate child kernel stack ───────────────────────────────
        let stack_base = match mm::buddy::alloc(1) {
            Some(a) => a,
            None    => return -12,
        };
        let stack_size = mm::buddy::PAGE_SIZE * 2; // 8 KiB
        unsafe { (stack_base as *mut u8).write_bytes(0, stack_size); }

        // ── Step 2: allocate child page-table root ────────────────────────────
        let child_pt = unsafe { super::arch_alloc_page_table_root() };
        if child_pt == 0 {
            mm::buddy::free(stack_base, 1);
            return -12;
        }

        // ── Step 3: clone the parent's address space ──────────────────────────
        //
        // We need a raw pointer to the parent's AddressSpace to clone it
        // without holding the run-queue lock across the (potentially slow)
        // page-copy loop.  This is safe in the cooperative scheduler because
        // the parent task cannot be rescheduled (preempted) between releasing
        // the lock and the clone: only the currently-running task can call
        // fork_current(), and cooperative tasks do not preempt each other.
        let as_raw_ptr: *const mm::vmm::AddressSpace = {
            let rq = super::RUN_QUEUE.lock();
            match rq.find_pid(parent_pid) {
                Some(t) => match t.address_space.as_ref() {
                    Some(as_) => as_ as *const mm::vmm::AddressSpace,
                    None => {
                        mm::buddy::free(stack_base, 1);
                        mm::buddy::free(child_pt, 0);
                        return -38; // kernel task → can't fork
                    }
                },
                None => {
                    mm::buddy::free(stack_base, 1);
                    mm::buddy::free(child_pt, 0);
                    return -3;
                }
            }
        };

        let child_as = unsafe {
            match mm::cow::clone_as(&*as_raw_ptr, child_pt) {
                Some(a) => a,
                None    => {
                    mm::buddy::free(stack_base, 1);
                    mm::buddy::free(child_pt, 0);
                    return -12;
                }
            }
        };

        // ── Step 4: copy UserFrame to top of child kernel stack ───────────────
        //
        // The frame is placed at [stack_base + stack_size - UserFrame::SIZE].
        // When the child is first scheduled, cpu_switch_to restores SP to that
        // address and ret-branches to ret_to_user_fork, which pops the frame
        // and eret's to user space.
        const FRAME_SIZE: usize = UserFrame::SIZE;
        let frame_offset    = stack_size - FRAME_SIZE;
        let child_frame_ptr = (stack_base + frame_offset) as *mut UserFrame;

        unsafe {
            core::ptr::copy_nonoverlapping(
                frame_ptr      as *const UserFrame,
                child_frame_ptr,
                1,
            );
            // Fork returns 0 in the child.
            (*child_frame_ptr).x[0] = 0;
        }

        // ── Step 5: build child CpuContext ────────────────────────────────────
        extern "C" { fn ret_to_user_fork(); }
        let mut child_ctx         = CpuContext::zeroed();
        child_ctx.gregs[11]       = ret_to_user_fork as *const () as u64; // LR
        child_ctx.sp              = (stack_base + frame_offset) as u64;

        // ── Step 6: gather parent credentials ────────────────────────────────
        let (heap_start, heap_end, ppid, tgid, pgid, sid, uid, gid, euid, egid) = {
            let rq = super::RUN_QUEUE.lock();
            if let Some(t) = rq.find_pid(parent_pid) {
                let (hs, he) = t.address_space.as_ref()
                    .map(|a| (a.heap_start, a.heap_end))
                    .unwrap_or((0, 0));
                (hs, he, t.pid, t.tgid, t.pgid, t.sid,
                 t.uid, t.gid, t.euid, t.egid)
            } else {
                mm::buddy::free(stack_base, 1);
                mm::buddy::free(child_pt, 0);
                return -3;
            }
        };

        // ── Step 7: build and enqueue child task ──────────────────────────────
        let child_pid = super::alloc_pid();

        let mut child = task::Task::new_kernel(
            child_pid, 0, stack_base, stack_size, child_pt,
        );
        child.ctx           = child_ctx;
        child.address_space = Some(child_as);
        child.ppid          = ppid;
        child.tgid          = tgid;
        child.pgid          = pgid;
        child.sid           = sid;
        child.uid           = uid;
        child.gid           = gid;
        child.euid          = euid;
        child.egid          = egid;
        child.heap_start    = heap_start;
        child.heap_end      = heap_end;
        child.signal_actions = [DEFAULT_SIGACTION; 4];

        if !super::RUN_QUEUE.lock().enqueue(child) {
            mm::buddy::free(stack_base, 1);
            mm::buddy::free(child_pt, 0);
            return -12;
        }

        child_pid as isize
    }
}

/// Spawn a new thread sharing the current process's virtual address space.
///
/// `flags`       — Linux CLONE_* flags (CLONE_VM must be set).
/// `child_stack` — top of the stack the child thread should use.
/// `tls`         — value to write into TPIDR_EL0 (AArch64) / FS.base (x86-64)
///                 when `CLONE_SETTLS` is present in `flags`.
/// `ctid`        — user-space address for CLONE_CHILD_SETTID / CLONE_CHILD_CLEARTID.
/// `frame_ptr`   — address of the parent's `UserFrame` on the kernel stack.
///
/// The child thread resumes at the same PC as the parent (instruction after
/// the `svc` / `syscall`) with x0 / rax = 0 and SP = `child_stack`, exactly
/// like a fork-then-return except it shares the parent's page table.
///
/// Returns the child thread's PID to the parent, or a negative errno.
pub fn clone_thread(
    flags:       usize,
    child_stack: usize,
    tls:         usize,
    ctid:        usize,
    frame_ptr:   usize,
) -> isize {
    const CLONE_SETTLS:         usize = 0x0008_0000;
    const CLONE_THREAD:         usize = 0x0001_0000;
    const CLONE_CHILD_SETTID:   usize = 0x0100_0000;
    const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;

    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (flags, child_stack, tls, ctid, frame_ptr);
        return -38; // ENOSYS
    }

    #[cfg(target_arch = "aarch64")]
    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
        if parent_pid == 0 { return -38; }

        // ── Allocate child kernel stack ───────────────────────────────────────
        let stack_base = match mm::buddy::alloc(1) {
            Some(a) => a,
            None    => return -12,
        };
        let stack_size = mm::buddy::PAGE_SIZE * 2;
        unsafe { (stack_base as *mut u8).write_bytes(0, stack_size); }

        // ── Copy parent's UserFrame to top of child kernel stack ──────────────
        const FRAME_SIZE: usize = UserFrame::SIZE;
        let frame_offset    = stack_size - FRAME_SIZE;
        let child_frame_ptr = (stack_base + frame_offset) as *mut UserFrame;

        unsafe {
            core::ptr::copy_nonoverlapping(
                frame_ptr as *const UserFrame,
                child_frame_ptr,
                1,
            );
            // Thread returns 0 from clone().
            (*child_frame_ptr).x[0] = 0;
            // Use the new thread stack instead of the parent's.
            if child_stack != 0 {
                (*child_frame_ptr).sp_el0 = child_stack as u64;
            }
        }

        // ── Build child CpuContext ────────────────────────────────────────────
        extern "C" { fn ret_to_user_fork(); }
        let mut child_ctx   = CpuContext::zeroed();
        child_ctx.gregs[11] = ret_to_user_fork as *const () as u64; // LR
        child_ctx.sp        = (stack_base + frame_offset) as u64;

        let child_tls = if flags & CLONE_SETTLS != 0 { tls as u64 } else { 0 };
        child_ctx.tpidr_el0 = child_tls;

        // ── Collect parent credentials and page table ─────────────────────────
        let (page_table, parent_tgid, pgid, sid, uid, gid, euid, egid, heap_start, heap_end,
             ctid_phys) = {
            let rq = super::RUN_QUEUE.lock();
            match rq.find_pid(parent_pid) {
                Some(t) => {
                    let cp = if flags & CLONE_CHILD_SETTID != 0 && ctid != 0 {
                        t.address_space.as_ref()
                            .and_then(|a| a.virt_to_phys(ctid))
                    } else {
                        None
                    };
                    let (hs, he) = t.address_space.as_ref()
                        .map(|a| (a.heap_start, a.heap_end))
                        .unwrap_or((0, 0));
                    (t.page_table, t.tgid, t.pgid, t.sid,
                     t.uid, t.gid, t.euid, t.egid, hs, he, cp)
                }
                None => {
                    mm::buddy::free(stack_base, 1);
                    return -3; // ESRCH
                }
            }
        };

        let child_pid = super::alloc_pid();

        // Write child PID to ctid (CLONE_CHILD_SETTID).
        if let Some(phys) = ctid_phys {
            unsafe { core::ptr::write(phys as *mut u32, child_pid); }
        }

        // ── Build and enqueue child task ──────────────────────────────────────
        //
        // Threads share the parent's page table but have NO owned AddressSpace.
        // This prevents double-freeing shared pages when the thread exits.
        // Page faults in the thread resolve against the already-mapped VMAs.
        let mut child = task::Task::new_kernel(
            child_pid, 0, stack_base, stack_size, page_table,
        );
        child.ctx        = child_ctx;
        child.tls_base   = child_tls;
        child.ppid       = parent_pid;
        child.tgid       = if flags & CLONE_THREAD != 0 { parent_tgid } else { child_pid };
        child.pgid       = pgid;
        child.sid        = sid;
        child.uid        = uid;  child.gid  = gid;
        child.euid       = euid; child.egid = egid;
        child.heap_start = heap_start;
        child.heap_end   = heap_end;
        child.signal_actions = [DEFAULT_SIGACTION; 4];
        if flags & CLONE_CHILD_CLEARTID != 0 {
            child.clear_child_tid = ctid;
        }
        // address_space stays None — thread shares page_table but does not own it.

        if !super::RUN_QUEUE.lock().enqueue(child) {
            mm::buddy::free(stack_base, 1);
            return -12;
        }

        child_pid as isize
    }
}
