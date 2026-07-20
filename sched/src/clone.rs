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
/// `before_enqueue` runs with the child's PID after the child task is fully
/// constructed but **before** it is made runnable.  The kernel uses it to
/// duplicate the parent's VFS fd table: on SMP another CPU can dispatch the
/// child the instant it is enqueued, and if the child's first syscall
/// (`pipe()`, `open()`) beats the fd-table clone, the VFS creates a fresh
/// empty table and hands the child fd 0/1 for regular files — aliasing
/// stdin/stdout.
///
/// Returns the child PID (> 0) to the parent, or a negative `errno` on error:
/// * `-12` ENOMEM  — OOM or run queue full
/// * `-38` ENOSYS  — architecture not supported
pub fn fork_current(frame_ptr: usize, before_enqueue: impl FnOnce(u32)) -> isize {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (frame_ptr, before_enqueue);
        return -38; // ENOSYS on other architectures
    }

    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = super::current_pid();
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
        // A mutable pointer is required (not just `&`): sharing a page
        // copy-on-write means downgrading the *parent's* own page-table
        // entries to read-only too, and marking its own VmaRegions as
        // CoW-tracked, not just the child's.  Exclusive access for the
        // whole clone comes from the per-address-space busy flag, NOT the
        // run-queue lock: the clone walks and remaps every VMA and ends in
        // a TLB-shootdown broadcast, far too long to pin every other CPU's
        // scheduler loop (see sched::lock_leader_address_space).  It also
        // serializes against concurrent page faults and mm syscalls by
        // other threads of the parent, which the old
        // pointer-with-lock-dropped scheme raced with on SMP.
        //
        // Distinguish "no such task" (-3) from "kernel task without an
        // address space" (-38) before taking the lock, since the lock
        // helper folds both into None.
        let parent_tgid_for_quiesce;
        {
            let rq = super::RUN_QUEUE.lock();
            match rq.find_pid(parent_pid).and_then(|t| rq.find_pid(t.tgid)) {
                Some(leader) => {
                    if leader.address_space.is_none() {
                        drop(rq);
                        mm::buddy::free(stack_base_phys, stack_pages);
                        mm::buddy::free(child_pt, 0);
                        return -38; // kernel task → can't fork
                    }
                    parent_tgid_for_quiesce = leader.pid;
                }
                None => {
                    drop(rq);
                    mm::buddy::free(stack_base_phys, stack_pages);
                    mm::buddy::free(child_pt, 0);
                    return -3; // ESRCH
                }
            }
        }
        // Stop-the-world across the CoW clone: sibling threads with stale
        // writable TLB entries must not run between the PTE downgrades and
        // the final broadcast shootdown (see quiesce_thread_group's doc).
        let quiesced =
            super::quiesce_thread_group(parent_tgid_for_quiesce, parent_pid);
        let as_raw_ptr: *mut mm::vmm::AddressSpace =
            match super::lock_leader_address_space(parent_pid) {
                Some(p) => p,
                None => {
                    if quiesced { super::unquiesce_thread_group(); }
                    mm::buddy::free(stack_base_phys, stack_pages);
                    mm::buddy::free(child_pt, 0);
                    return -3; // ESRCH — task vanished since the check above
                }
            };

        let cloned = unsafe { mm::cow::clone_as(&mut *as_raw_ptr, child_pt) };
        unsafe { super::unlock_address_space(as_raw_ptr); }
        if quiesced { super::unquiesce_thread_group(); }
        let child_as = match cloned {
            Some(a) => a,
            None    => {
                mm::buddy::free(stack_base_phys, stack_pages);
                mm::buddy::free(child_pt, 0);
                return -12;
            }
        };

        // ── Step 4: copy UserFrame to top of child kernel stack ───────────────
        // The frame base becomes the child's kernel SP (ctx.sp on aarch64),
        // so it must stay 16-byte aligned: with SCTLR_EL1.SA=1 real hardware
        // (HVF) takes an SP-alignment fault on any SP-based access whose SP
        // isn't 16-byte aligned, and the raw UserFrame size (280) is not a
        // multiple of 16. QEMU TCG doesn't model this check, which masked the
        // bug. 288 also matches exception_asm.s's `sub sp, sp, #288` frame.
        const FRAME_SIZE: usize = (UserFrame::SIZE + 15) & !15;
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
            unsafe {
                (*child_frame_ptr).x[0] = 0;           // fork returns 0 to child
                (*child_frame_ptr).pt = child_pt as u64; // child must use its own page table
            }
            child_ctx.gregs[11] = ret_to_user_fork as *const () as u64; // LR
            child_ctx.sp = (stack_base_virt + frame_offset) as u64;
        }

        #[cfg(target_arch = "x86_64")]
        {
            extern "C" { fn fork_ret_to_user(); }
            unsafe {
                (*child_frame_ptr).rax = 0; // Return 0 to child
            }

            // Initial child RSP for context switch:
            // CpuContext::cpu_switch_to will 'ret' to the target address on the stack.
            // We place 'fork_ret_to_user' right below the UserFrame.
            let child_ksp_virt = (child_frame_ptr as usize).wrapping_sub(8);
            unsafe {
                let p = child_ksp_virt as *mut u64;
                p.write(fork_ret_to_user as *const () as u64);
            }
            child_ctx.rsp = child_ksp_virt as u64;
        }

        // ── Step 6: gather parent credentials ────────────────────────────────
        let (heap_start, heap_end, pid, _tgid, pgid, sid, uid, gid, euid, egid, cwd, tls_base,
             nice, umask, root) = {
            let rq = super::RUN_QUEUE.lock();
            if let Some(t) = rq.find_pid(parent_pid) {
                let leader = rq.find_pid(t.tgid).unwrap_or(t);
                let (hs, he) = leader.address_space.as_ref()
                    .map(|a| (a.heap_start, a.heap_end))
                    .unwrap_or((0, 0));
                (hs, he, t.pid, t.tgid, t.pgid, t.sid,
                 t.uid, t.gid, t.euid, t.egid, (t.cwd.clone(), t.cwd_len), t.tls_base,
                 t.priority, t.umask, (t.root.clone(), t.root_len))
            } else {
                mm::buddy::free(stack_base_phys, stack_pages);
                mm::buddy::free(child_pt, 0);
                return -3;
            }
        };

        // fork() duplicates the whole process, TLS included — unlike
        // clone_thread's fresh `child_tls`, the child must keep running with
        // the exact same TLS base the parent had at the moment of the fork
        // syscall. child_ctx started at CpuContext::zeroed(), so without
        // this the child's first #[thread_local] access (errno, etc.) reads
        // through a null TLS base and faults.
        //
        // On x86-64 the kernel maintains the TLS base itself (arch_prctl
        // ARCH_SET_FS traps into set_fs_base), so Task::tls_base is
        // authoritative and used directly.
        //
        // On AArch64 there is NO such trap: musl (and any libc that follows
        // the aarch64 ABI) installs the main thread's TLS with a bare
        // `msr tpidr_el0` from EL0, which the kernel never observes, so the
        // Task::tls_base shadow field stays 0 for the whole process lifetime
        // (see context::current_tls_base's doc comment). Copying that 0 into
        // the child gives it a NULL TLS base. The live register is the only
        // source of truth, so read it directly — exactly as clone_thread
        // already does for its vfork-style fallback. This was masked for the
        // common fork-then-immediately-execve case (arch_execve_return zeroes
        // tpidr_el0 anyway, and musl's raw fork+exec child touches no TLS),
        // but Rust std's fork+exec child runs #[thread_local]-touching Rust
        // code *between* fork and execve, so it faulted on the null base
        // (e.g. brush -c spawning an external command).
        #[cfg(target_arch = "x86_64")]
        { child_ctx.fs_base = tls_base; }
        #[cfg(target_arch = "aarch64")]
        { child_ctx.tpidr_el0 = crate::context::current_tls_base(); }

        // ── Step 7: build and enqueue child task ──────────────────────────────
        let child_pid = super::alloc_pid();

        let mut child = task::Task::new_kernel(
            child_pid, 0, stack_base_phys, stack_size, child_pt,
        );
        child.ctx           = child_ctx;
        child.tls_base      = tls_base;
        child.address_space = Some(alloc::sync::Arc::new(child_as));
        child.ppid          = pid;
        child.tgid          = child_pid;
        child.pgid          = pgid;
        child.sid           = sid;
        child.uid           = uid;
        child.gid           = gid;
        child.euid          = euid;
        child.egid          = egid;
        child.heap_start    = heap_start;
        child.heap_end      = heap_end;
        // The cwd is a (bytes, len) pair: `cwd` alone is a fixed 128-byte
        // array whose tail is garbage, and `Task::new_kernel` initialises
        // `cwd_len` to 1 ("/"). Copying only the bytes left every forked
        // child with an effective cwd of "/" no matter where the parent had
        // chdir'd, so `cd /tmp; prog a.txt` resolved to "/a.txt" in the
        // child (and getcwd() in the child answered "/").
        child.cwd           = cwd.0;
        child.cwd_len       = cwd.1;
        // POSIX: a child inherits the parent's nice value. `Task::new_*` builds
        // every task at nice 0, so without this a `nice -n 10 sh -c ...` lost
        // the niceness at the first fork — i.e. for everything the shell
        // actually ran.
        child.priority      = nice;
        child.weight        = task::nice_to_weight(nice);
        // umask is inherited too. Task::new_* hardcodes 0o022, so a child of a
        // process that had set its own mask silently reverted to the default.
        child.umask         = umask;
        // A chrooted parent's children stay in the jail.
        child.root          = root.0;
        child.root_len      = root.1;
        child.signal_actions = [DEFAULT_SIGACTION; 64];

        // Give the caller its chance to set up per-child kernel state (VFS
        // fd table) while the child is still invisible to other CPUs.
        before_enqueue(child_pid);

        if !super::RUN_QUEUE.lock().enqueue(child) {
            mm::buddy::free(stack_base_phys, stack_pages);
            mm::buddy::free(child_pt, 0);
            return -12;
        }
        super::wake_up_an_idle_cpu();

        child_pid as isize
    }
}

/// Spawn a new thread sharing the current process's virtual address space.
///
/// `before_enqueue` mirrors `fork_current`'s hook of the same name: it runs
/// with the child's PID after construction but before the child is made
/// runnable, so the caller can duplicate per-process kernel-side state (the
/// VFS fd table) with no window for the child to run a syscall against a
/// table that doesn't exist yet. Only used for the non-`CLONE_THREAD`
/// (vfork-style) case in practice — see the call in `clone_thread`'s body.
pub fn clone_thread(
    flags:       usize,
    child_stack: usize,
    #[allow(unused_variables)]
    tls:         usize,
    ctid:        usize,
    frame_ptr:   usize,
    before_enqueue: impl FnOnce(u32),
) -> isize {
    #[allow(dead_code)]
    const CLONE_SETTLS:         usize = 0x0008_0000;
    const CLONE_THREAD:         usize = 0x0001_0000;
    const CLONE_CHILD_SETTID:   usize = 0x0100_0000;
    const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;
    const CLONE_VFORK:          usize = 0x0000_4000;

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (flags, child_stack, tls, ctid, frame_ptr);
        return -38; // ENOSYS
    }

    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = super::current_pid();
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
        // Round up to keep the child's kernel SP 16-byte aligned — see the
        // matching comment in fork_current (SCTLR_EL1.SA fault under HVF).
        const FRAME_SIZE: usize = (UserFrame::SIZE + 15) & !15;
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
        // Real Linux clone() always inherits the caller's *current* TLS base
        // into the child; CLONE_SETTLS only *overrides* it with a caller-
        // supplied one, it's never the difference between "some TLS" and
        // "no TLS" (see fork_current's identical t.tls_base carry-over,
        // which covers the CLONE_VM-clear fork() case). Forcing 0 here for
        // any clone() that omits CLONE_SETTLS broke vfork()-style spawns —
        // musl's Command::spawn() posix_spawn fast path calls
        // clone(CLONE_VM|CLONE_VFORK|SIGCHLD) with no TLS args at all,
        // expecting the child to keep running with the parent's live TLS
        // block (e.g. for errno) until it execve()s or _exit()s. Zeroing it
        // null-derefs on the child's very first thread-local access.
        // Read the register directly (crate::context::current_tls_base), not
        // Task::tls_base — that shadow field is never populated on AArch64
        // (see current_tls_base's doc comment), and reading it here would
        // silently reintroduce the same null-TLS crash on that arch alone.
        let child_tls = if flags & CLONE_SETTLS != 0 { tls as u64 } else { crate::context::current_tls_base() };

        #[cfg(target_arch = "aarch64")]
        {
            extern "C" { fn ret_to_user_fork(); }
            unsafe {
                (*child_frame_ptr).x[0] = 0;
                if child_stack != 0 { (*child_frame_ptr).sp_el0 = child_stack as u64; }
            }
            child_ctx.gregs[11] = ret_to_user_fork as *const () as u64; // LR
            child_ctx.sp        = (stack_base_virt + frame_offset) as u64;
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
            let child_ksp = (child_frame_ptr as usize).wrapping_sub(8);
            unsafe {
                let p = child_ksp as *mut u64;
                p.write(fork_ret_to_user as *const () as u64);
            }
            child_ctx.rsp = child_ksp as u64;
            child_ctx.fs_base = child_tls;
        }

        // ── Collect parent credentials and page table ─────────────────────────
        let (page_table, parent_tgid, pgid, sid, uid, gid, euid, egid, heap_start, heap_end,
             ctid_phys, cwd, leader_as, nice, umask, root) = {
            let rq = super::RUN_QUEUE.lock();
            match rq.find_pid(parent_pid) {
                Some(t) => {
                    let leader = rq.find_pid(t.tgid).unwrap_or(t);
                    let cp = if flags & CLONE_CHILD_SETTID != 0 && ctid != 0 {
                        leader.address_space.as_ref()
                            .and_then(|a| a.virt_to_phys(ctid))
                    } else {
                        None
                    };
                    let (hs, he) = leader.address_space.as_ref()
                        .map(|a| (a.heap_start, a.heap_end))
                        .unwrap_or((0, 0));
                    // Cheap Arc clone (refcount bump) — handed to non-
                    // CLONE_THREAD (vfork-style) children below. Real
                    // CLONE_THREAD siblings don't need it: they share the
                    // leader's tgid, so lock_leader_address_space's tgid
                    // lookup already resolves to it.
                    (t.page_table, t.tgid, t.pgid, t.sid,
                     t.uid, t.gid, t.euid, t.egid, hs, he, cp, (t.cwd.clone(), t.cwd_len),
                     leader.address_space.clone(), t.priority, t.umask, (t.root.clone(), t.root_len))
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
        child.tls_base   = child_tls;
        child.ppid       = parent_pid;
        child.tgid       = if flags & CLONE_THREAD != 0 { parent_tgid } else { child_pid };
        // Vfork-style children (CLONE_VM without CLONE_THREAD — musl/std's
        // Command::spawn fast path) get their own tgid above, so they can't
        // ride the leader's tgid lookup the way real CLONE_THREAD siblings
        // do (see lock_leader_address_space). Without this, any real page
        // fault the child takes (not just the deliberate exec-failure
        // poison fault) hits "no address space for faulting task" and gets
        // killed. Aliasing the same Arc — not a copy — is required: the
        // whole point of CLONE_VM is that parent and child share one
        // address space until the child execve()s or exits.
        if flags & CLONE_THREAD == 0 {
            child.address_space = leader_as;
        }
        child.pgid       = pgid;
        child.sid        = sid;
        child.uid        = uid;  child.gid  = gid;
        child.euid       = euid; child.egid = egid;
        child.heap_start = heap_start;
        child.heap_end   = heap_end;
        // See fork_current: cwd is (bytes, len); the length must travel too.
        child.cwd        = cwd.0;
        child.cwd_len    = cwd.1;
        // See fork_current: nice is inherited. On Linux it is per-thread, so a
        // new thread starts at its creator's value rather than the group's.
        child.priority   = nice;
        child.weight     = task::nice_to_weight(nice);
        child.umask      = umask;
        child.root       = root.0;
        child.root_len   = root.1;
        child.signal_actions = [DEFAULT_SIGACTION; 64];
        child.vfork_pending = flags & CLONE_VFORK != 0;
        if flags & CLONE_CHILD_CLEARTID != 0 {
            child.clear_child_tid = ctid;
        }

        // Give the caller its chance to set up per-child kernel state (VFS
        // fd table) while the child is still invisible to other CPUs — same
        // SMP race fork_current's doc comment describes: another CPU could
        // otherwise dispatch the child immediately and lose the race against
        // its own first fd-allocating syscall.
        before_enqueue(child_pid);

        if !super::RUN_QUEUE.lock().enqueue(child) {
            mm::buddy::free(stack_base_phys, stack_pages);
            return -12;
        }
        super::wake_up_an_idle_cpu();

        // CLONE_VFORK: suspend the parent until the child execve()s or
        // exits. Under CLONE_VM the child shares this address space — musl's
        // posix_spawn even runs it on a buffer inside the parent's stack
        // frame — so letting the parent run early means both sides corrupt
        // each other's stack. No EINTR here: vfork isn't restartable.
        if flags & CLONE_VFORK != 0 {
            loop {
                let pending = {
                    let rq = super::RUN_QUEUE.lock();
                    match rq.find_pid(child_pid) {
                        Some(t) => t.vfork_pending,
                        None    => false, // already reaped — definitely done
                    }
                };
                if !pending { break; }
                super::irq_window();
                super::yield_now("vfork_wait");
            }
        }

        child_pid as isize
    }
}
