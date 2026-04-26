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
//! The x86-64 SYSCALL path saves a full `UserFrame` on the kernel stack
//! before calling `syscall_dispatch`. `fork_current` copies this frame to the
//! child's kernel stack and sets up the child's context to return via
//! `fork_ret_to_user`.

use crate::task::{self, DEFAULT_SIGACTION};

/// Perform a POSIX `fork()`.
///
/// `frame_ptr` — virtual address of the `UserFrame` saved on the parent's
/// kernel stack by the exception entry stub.
///
/// Returns the child PID (> 0) to the parent, or a negative `errno` on error:
/// * `-12` ENOMEM  — OOM or run queue full
/// * `-38` ENOSYS  — architecture not supported
pub fn fork_current(frame_ptr: usize) -> isize {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = frame_ptr;
        return -38; // ENOSYS on other architectures
    }

    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
        if parent_pid == 0 { return -38; }

        // ── Step 1: allocate child kernel stack ───────────────────────────────
        let stack_pages = 4; // 64 KiB
        let stack_base_phys = match mm::buddy::alloc(stack_pages) {
            Some(a) => a,
            None    => return -12, // ENOMEM
        };
        let stack_size = mm::buddy::PAGE_SIZE * (1 << stack_pages);
        let stack_base_virt = mm::phys_to_virt(stack_base_phys);
        unsafe { (stack_base_virt as *mut u8).write_bytes(0, stack_size); }

        // ── Step 2: allocate child page-table root ────────────────────────────
        let child_pt = unsafe { super::arch_alloc_page_table_root() };
        if child_pt == 0 {
            mm::buddy::free(stack_base_phys, stack_pages);
            return -12;
        }

        // ── Step 3: clone the parent's address space (COW) ────────────────────
        let as_raw_ptr: *const mm::vmm::AddressSpace = {
            let rq = super::RUN_QUEUE.lock();
            match rq.find_pid(parent_pid) {
                Some(t) => match t.address_space.as_ref() {
                    Some(as_) => &**as_ as *const mm::vmm::AddressSpace,
                    None => {
                        mm::buddy::free(stack_base_phys, 3);
                        mm::buddy::free(child_pt, 0);
                        return -38; // kernel task → can't fork
                    }
                    },
                    None => {
                    mm::buddy::free(stack_base_phys, 3);
                    mm::buddy::free(child_pt, 0);
                    return -3; // ESRCH
                    }

            }
        };

        let child_as = unsafe {
            match mm::cow::clone_as(&*as_raw_ptr, child_pt) {
                Some(a) => a,
                None    => {
                    mm::buddy::free(stack_base_phys, stack_pages);
                    mm::buddy::free(child_pt, 0);
                    return -12;
                }
            }
        };

        // ── Step 4: copy UserFrame to top of child kernel stack ───────────────
        const FRAME_SIZE: usize = UserFrame::SIZE;
        let frame_offset    = stack_size - FRAME_SIZE;
        let child_frame_ptr = (stack_base_virt + frame_offset) as *mut UserFrame;

        unsafe {
            core::ptr::copy_nonoverlapping(
                frame_ptr      as *const UserFrame,
                child_frame_ptr,
                1,
            );
        }

        // ── Step 5: build child CpuContext ────────────────────────────────────
        let mut child_ctx = CpuContext::zeroed();

        #[cfg(target_arch = "aarch64")]
        {
            extern "C" { fn ret_to_user_fork(); }
            unsafe { (*child_frame_ptr).x[0] = 0; } // Return 0 to child
            child_ctx.gregs[11] = ret_to_user_fork as *const () as u64; // LR
            child_ctx.sp = (stack_base_virt + frame_offset) as u64;
        }

        #[cfg(target_arch = "x86_64")]
        {
            extern "C" { fn fork_ret_to_user(); }
            unsafe { (*child_frame_ptr).rax = 0; } // Return 0 to child

            // Initial child RSP for context switch:
            // CpuContext::cpu_switch_to expects RSP to point to its frame:
            // [r15, r14, r13, r12, rbp, rbx, ret_target]
            // This is 7 words total. We place it right below the UserFrame.
            let child_ksp_virt = (child_frame_ptr as usize).wrapping_sub(7 * 8);
            unsafe {
                let p = child_ksp_virt as *mut u64;
                // Pop order in cpu_switch_to: r15, r14, r13, r12, rbp, rbx, then 'ret'
                p.add(0).write(0); // r15
                p.add(1).write(0); // r14
                p.add(2).write(0); // r13
                p.add(3).write(0); // r12
                p.add(4).write(0); // rbp
                p.add(5).write(0); // rbx
                p.add(6).write(fork_ret_to_user as *const () as u64); // return target
            }
            child_ctx.rsp = child_ksp_virt as u64;
        }

        // ── Step 6: gather parent credentials ────────────────────────────────
        let (heap_start, heap_end, pid, tgid, pgid, sid, uid, gid, euid, egid, cwd) = {
            let rq = super::RUN_QUEUE.lock();
            if let Some(t) = rq.find_pid(parent_pid) {
                let (hs, he) = t.address_space.as_ref()
                    .map(|a| (a.heap_start, a.heap_end))
                    .unwrap_or((0, 0));
                (hs, he, t.pid, t.tgid, t.pgid, t.sid,
                 t.uid, t.gid, t.euid, t.egid, t.cwd.clone())
            } else {
                mm::buddy::free(stack_base_phys, stack_pages);
                mm::buddy::free(child_pt, 0);
                return -3;
            }
        };

        // ── Step 7: build and enqueue child task ──────────────────────────────
        let child_pid = super::alloc_pid();

        let mut child = task::Task::new_kernel(
            child_pid, 0, stack_base_phys, stack_size, child_pt,
        );
        child.ctx           = child_ctx;
        child.address_space = Some(alloc::boxed::Box::new(child_as));
        child.ppid          = pid;
        child.tgid          = tgid;
        child.pgid          = pgid;
        child.sid           = sid;
        child.uid           = uid;
        child.gid           = gid;
        child.euid          = euid;
        child.egid          = egid;
        child.heap_start    = heap_start;
        child.heap_end      = heap_end;
        child.cwd           = cwd;
        child.signal_actions = [DEFAULT_SIGACTION; 4];

        if !super::RUN_QUEUE.lock().enqueue(child) {
            mm::buddy::free(stack_base_phys, stack_pages);
            mm::buddy::free(child_pt, 0);
            return -12;
        }

        child_pid as isize
    }
}

/// Spawn a new thread sharing the current process's virtual address space.
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

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (flags, child_stack, tls, ctid, frame_ptr);
        return -38; // ENOSYS
    }

    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
        if parent_pid == 0 { return -38; }

        // ── Allocate child kernel stack ───────────────────────────────────────
        let stack_pages = 4; // 64 KiB
        let stack_base_phys = match mm::buddy::alloc(stack_pages) {
            Some(a) => a,
            None    => return -12,
        };
        let stack_size = mm::buddy::PAGE_SIZE * (1 << stack_pages);
        let stack_base_virt = mm::phys_to_virt(stack_base_phys);
        unsafe { (stack_base_virt as *mut u8).write_bytes(0, stack_size); }

        // ── Copy parent's UserFrame to top of child kernel stack ──────────────
        const FRAME_SIZE: usize = UserFrame::SIZE;
        let frame_offset    = stack_size - FRAME_SIZE;
        let child_frame_ptr = (stack_base_virt + frame_offset) as *mut UserFrame;

        unsafe {
            core::ptr::copy_nonoverlapping(
                frame_ptr as *const UserFrame,
                child_frame_ptr,
                1,
            );
        }

        // ── Build child CpuContext ────────────────────────────────────────────
        let mut child_ctx = CpuContext::zeroed();

        #[cfg(target_arch = "aarch64")]
        {
            extern "C" { fn ret_to_user_fork(); }
            unsafe {
                (*child_frame_ptr).x[0] = 0;
                if child_stack != 0 { (*child_frame_ptr).sp_el0 = child_stack as u64; }
            }
            child_ctx.gregs[11] = ret_to_user_fork as *const () as u64; // LR
            child_ctx.sp        = (stack_base_virt + frame_offset) as u64;
            let child_tls = if flags & CLONE_SETTLS != 0 { tls as u64 } else { 0 };
            child_ctx.tpidr_el0 = child_tls;
        }

        #[cfg(target_arch = "x86_64")]
        {
            extern "C" { fn fork_ret_to_user(); }
            unsafe {
                (*child_frame_ptr).rax = 0;
                if child_stack != 0 { (*child_frame_ptr).rsp = child_stack as u64; }
            }

            // Initial child RSP for context switch
            let child_ksp = (child_frame_ptr as usize).wrapping_sub(7 * 8);
            unsafe {
                let p = child_ksp as *mut u64;
                p.add(0).write(0); p.add(1).write(0); p.add(2).write(0);
                p.add(3).write(0); p.add(4).write(0); p.add(5).write(0);
                p.add(6).write(fork_ret_to_user as *const () as u64);
            }
            child_ctx.rsp = child_ksp as u64;

            // TODO: FS.base setup for CLONE_SETTLS on x86_64
        }

        // ── Collect parent credentials and page table ─────────────────────────
        let (page_table, parent_tgid, pgid, sid, uid, gid, euid, egid, heap_start, heap_end,
             ctid_phys, cwd) = {
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
                     t.uid, t.gid, t.euid, t.egid, hs, he, cp, t.cwd.clone())
                }
                None => {
                    mm::buddy::free(stack_base_phys, stack_pages);
                    return -3; // ESRCH
                }
            }
        };

        let child_pid = super::alloc_pid();

        // Write child PID to ctid (CLONE_CHILD_SETTID).
        if let Some(phys) = ctid_phys {
            let virt = mm::phys_to_virt(phys);
            unsafe { core::ptr::write(virt as *mut u32, child_pid); }
        }

        // ── Build and enqueue child task ──────────────────────────────────────
        let mut child = task::Task::new_kernel(
            child_pid, 0, stack_base_phys, stack_size, page_table,
        );
        child.ctx        = child_ctx;
        child.ppid       = parent_pid;
        child.tgid       = if flags & CLONE_THREAD != 0 { parent_tgid } else { child_pid };
        child.pgid       = pgid;
        child.sid        = sid;
        child.uid        = uid;  child.gid  = gid;
        child.euid       = euid; child.egid = egid;
        child.heap_start = heap_start;
        child.heap_end   = heap_end;
        child.cwd        = cwd;
        child.signal_actions = [DEFAULT_SIGACTION; 4];
        if flags & CLONE_CHILD_CLEARTID != 0 {
            child.clear_child_tid = ctid;
        }

        if !super::RUN_QUEUE.lock().enqueue(child) {
            mm::buddy::free(stack_base_phys, stack_pages);
            return -12;
        }

        child_pid as isize
    }
}
