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

/// Report a full task table once per boot. `fork`/`clone` return ENOMEM here
/// with any amount of physical memory free, and userspace only ever sees the
/// errno — brush prints "Out of memory (os error 12)" and nothing else, which
/// has already sent one investigation looking for a memory leak that was not
/// there. Once is enough: the table stays full, so an ungated print would be
/// one line per failed spawn on a console that costs ~0.19 s per line.
fn report_task_table_full() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    extern "C" {
        fn arch_serial_putc(c: u8);
        fn print_number(n: u32);
    }
    let (len, cap) = super::task_census();
    for &b in b"\n[SCHED] task table FULL: " {
        unsafe { arch_serial_putc(b) };
    }
    unsafe { print_number(len as u32) };
    for &b in b"/" {
        unsafe { arch_serial_putc(b) };
    }
    unsafe { print_number(cap as u32) };
    for &b in b" tasks -- fork/clone now return ENOMEM; this is runqueue::MAX_TASKS, not RAM\n" {
        unsafe { arch_serial_putc(b) };
    }
}

/// Copy the signal-disposition table of thread group `parent_tgid` into a
/// freshly built child process. A short RUN_QUEUE hold of its own, after the
/// child exists, so the 2 KiB table is copied straight into the child's
/// heap allocation rather than staged through a stack temporary.
fn inherit_signal_actions(child: &mut task::Task, parent_tgid: u32) {
    let rq = super::RUN_QUEUE.lock();
    if let Some(leader) = rq.find_pid(parent_tgid) {
        child.signal_actions = leader.signal_actions;
    }
}

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
        let stack_pages = super::KERNEL_STACK_ORDER;
        let stack_base_phys = match mm::buddy::alloc(stack_pages) {
            Some(a) => a,
            None    => return -12, // ENOMEM
        };
        let stack_size = super::KERNEL_STACK_SIZE;
        let stack_base_virt = mm::phys_to_virt(stack_base_phys);
        unsafe { (stack_base_virt as *mut u8).write_bytes(0, stack_size); }
        super::arm_kernel_stack(stack_base_phys);

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
                // `clone_as` builds the child around `child_pt` as a local
                // `AddressSpace`; on OOM it returns None having already
                // dropped that AddressSpace, whose `Drop` freed `child_pt`
                // (and every child page it had mapped). Freeing `child_pt`
                // again here is a double free that corrupts the buddy free
                // list — the latent cause of the intermittent `Vector=0xE`
                // in a later `buddy::free` on a large guest, where the OOM
                // rollback path actually runs. Only the kernel stack, which
                // no AddressSpace owns, is freed here.
                mm::buddy::free(stack_base_phys, stack_pages);
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
        let (heap_start, heap_end, pid, parent_tgid, pgid, sid, uid, gid, euid, egid, suid, sgid, cwd, tls_base,
             nice, umask, root, signal_mask, groups) = {
            let rq = super::RUN_QUEUE.lock();
            if let Some(t) = rq.find_pid(parent_pid) {
                let leader = rq.find_pid(t.tgid).unwrap_or(t);
                let (hs, he) = leader.address_space.as_ref()
                    .map(|a| (a.heap_start, a.heap_end))
                    .unwrap_or((0, 0));
                (hs, he, t.pid, t.tgid, t.pgid, t.sid,
                 t.uid, t.gid, t.euid, t.egid, t.suid, t.sgid, (t.cwd.clone(), t.cwd_len), t.tls_base,
                 t.priority, t.umask, (t.root.clone(), t.root_len), t.signal_mask, (t.ngroups, t.groups))
            } else {
                // `child_as` owns `child_pt` and is dropped on this return,
                // which frees it; an explicit free here would double it.
                mm::buddy::free(stack_base_phys, stack_pages);
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
        child.suid          = suid;
        child.sgid          = sgid;
        child.ngroups       = groups.0;
        child.groups        = groups.1;
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
        // POSIX fork(2): the child inherits the calling *thread's* signal
        // mask and the process's signal dispositions; only the pending set
        // starts empty (which `Task::new_kernel` already guarantees).
        //
        // Both used to be reset here. The mask reset broke the standard
        // "block SIGTERM/SIGINT in main, then spawn workers + signalfd"
        // pattern (tokio's signal driver, D-Bus daemons): every child and
        // worker came up with an empty mask, so the signal the parent had
        // carefully blocked killed the child instead of reaching the
        // signalfd. The disposition reset meant a SIG_IGN set by the parent
        // (a `nohup`-style SIGHUP, Rust std's SIGPIPE) never reached the
        // child, and a handler installed before fork was silently gone in a
        // child that did not exec.
        child.signal_mask   = signal_mask;
        inherit_signal_actions(&mut child, parent_tgid);

        // The child is a new process (its tgid == child_pid); inherit the
        // parent's /proc/self/exe path until it execs.
        super::inherit_exe_path(parent_tgid, child_pid);

        // Give the caller its chance to set up per-child kernel state (VFS
        // fd table) while the child is still invisible to other CPUs.
        before_enqueue(child_pid);

        if !super::RUN_QUEUE.lock().enqueue(child) {
            report_task_table_full();
            // A failed `enqueue` drops the `Box<Task>` it was handed, which
            // drops the child's address-space Arc and frees `child_pt`. The
            // kernel stack is not owned by the Task (the reaper frees it
            // explicitly), so it — and only it — is freed here.
            mm::buddy::free(stack_base_phys, stack_pages);
            return -12;
        }
        super::wake_up_an_idle_cpu();

        child_pid as isize
    }
}

/// Spawn a new thread sharing the current process's virtual address space.
///
/// `ptid` is the `CLONE_PARENT_SETTID` word: the child's tid is stored there,
/// in the shared address space, before the child becomes runnable. musl's
/// `pthread_create` relies on it for `new->tid` — it never assigns the
/// clone() return value itself — and `__tl_lock` keys the thread-list lock on
/// `__pthread_self()->tid`. A thread left with tid 0 treats a free lock
/// (value 0) as one it already holds recursively, bumping the process-wide
/// `tl_lock_count` without acquiring; when that thread exits (`pthread_exit`
/// takes the lock and leaves it for the kernel's CLEARTID to release) the
/// count stays high, the next real `__tl_lock`/`__tl_unlock` pair by the
/// main thread decrements the count instead of storing 0, and the lock is
/// held by the main thread forever. The first `fork()` from another thread
/// then parks in `__tl_lock` holding `__malloc_lock`, and the process wedges
/// — cosmic-comp's first keybinding spawn.
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
    ptid:        usize,
    ctid:        usize,
    frame_ptr:   usize,
    before_enqueue: impl FnOnce(u32),
) -> isize {
    #[allow(dead_code)]
    const CLONE_SETTLS:         usize = 0x0008_0000;
    const CLONE_THREAD:         usize = 0x0001_0000;
    const CLONE_PARENT_SETTID:  usize = 0x0010_0000;
    const CLONE_CHILD_SETTID:   usize = 0x0100_0000;
    const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;
    const CLONE_VFORK:          usize = 0x0000_4000;

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (flags, child_stack, tls, ptid, ctid, frame_ptr);
        return -38; // ENOSYS
    }

    {
        use crate::context::{CpuContext, UserFrame};

        if frame_ptr == 0 { return -38; }

        let parent_pid = super::current_pid();
        if parent_pid == 0 { return -38; }

        // ── Allocate child kernel stack ───────────────────────────────────────
        let stack_pages = super::KERNEL_STACK_ORDER;
        let stack_base_phys = match mm::buddy::alloc(stack_pages) {
            Some(a) => a,
            None    => return -12,
        };
        let stack_size = super::KERNEL_STACK_SIZE;
        let stack_base_virt = mm::phys_to_virt(stack_base_phys);
        unsafe { (stack_base_virt as *mut u8).write_bytes(0, stack_size); }
        super::arm_kernel_stack(stack_base_phys);

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
        let (page_table, parent_tgid, pgid, sid, uid, gid, euid, egid, suid, sgid, heap_start, heap_end,
             ctid_phys, ptid_phys, cwd, leader_as, nice, umask, root, signal_mask, groups) = {
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
                    // Resolved through the VMA tables like `cp`: no user
                    // access under RUN_QUEUE. The caller prefaults the word,
                    // so a None here means a bad pointer, not a lazy page.
                    let pp = if flags & CLONE_PARENT_SETTID != 0 && ptid != 0 {
                        leader.address_space.as_ref()
                            .and_then(|a| a.virt_to_phys(ptid))
                    } else {
                        None
                    };
                    let (hs, he) = leader.address_space.as_ref()
                        .map(|a| (a.heap_start, a.heap_end))
                        .unwrap_or((0, 0));
                    // Cheap Arc clone (refcount bump) — handed to every
                    // child below, CLONE_THREAD siblings included (see the
                    // note at the assignment).
                    (t.page_table, t.tgid, t.pgid, t.sid,
                     t.uid, t.gid, t.euid, t.egid, t.suid, t.sgid, hs, he, cp, pp, (t.cwd.clone(), t.cwd_len),
                     leader.address_space.clone(), t.priority, t.umask, (t.root.clone(), t.root_len),
                     t.signal_mask, (t.ngroups, t.groups))
                }
                None => {
                    mm::buddy::free(stack_base_phys, stack_pages);
                    return -3; // ESRCH
                }
            }
        };

        let child_pid = super::alloc_pid();

        // Write child PID to ctid (CLONE_CHILD_SETTID) and ptid
        // (CLONE_PARENT_SETTID). Both land before the child is enqueued, so
        // neither side can observe a stale word: under CLONE_VM the parent's
        // store is the child's view too.
        if let Some(phys) = ctid_phys {
            let virt = mm::phys_to_virt(phys);
            unsafe { core::ptr::write_volatile(virt as *mut u32, child_pid); }
        }
        if let Some(phys) = ptid_phys {
            let virt = mm::phys_to_virt(phys);
            unsafe { core::ptr::write_volatile(virt as *mut u32, child_pid); }
        }

        // ── Build and enqueue child task ──────────────────────────────────────
        let mut child = task::Task::new_kernel(
            child_pid, 0, stack_base_phys, stack_size, page_table,
        );
        child.ctx        = child_ctx;
        child.tls_base   = child_tls;
        child.ppid       = parent_pid;
        child.tgid       = if flags & CLONE_THREAD != 0 { parent_tgid } else { child_pid };
        // Every child aliases the same Arc — not a copy: the whole point of
        // CLONE_VM is that parent and child share one address space until
        // the child execve()s or exits.
        //
        // Vfork-style children (CLONE_VM without CLONE_THREAD — musl/std's
        // Command::spawn fast path) get their own tgid above, so they can't
        // ride the leader's tgid lookup (see lock_leader_address_space);
        // without their own reference any real page fault the child takes
        // hit "no address space for faulting task" and killed it.
        //
        // CLONE_THREAD siblings hold a reference too, and that is what keeps
        // `kill -9` of a threaded process from tearing the page tables out
        // from under a thread still executing on another CPU. Only the
        // leader used to own the space; a non-leader thread running the
        // group kill (`exit_group` from whichever thread the signal landed
        // on) could reap the leader while it, and other siblings, were still
        // running on those tables — a freed root under a live CPU. The
        // space is now freed when the *last* thread's Task drops, which the
        // scheduler only does once that thread is off its CPU and the CPU
        // has switched back to the kernel page table.
        child.address_space = leader_as;
        child.pgid       = pgid;
        child.sid        = sid;
        child.uid        = uid;  child.gid  = gid;
        child.euid       = euid; child.egid = egid;
        child.suid       = suid; child.sgid = sgid;
        child.ngroups    = groups.0; child.groups = groups.1;
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
        // A new thread starts with its creator's signal mask (POSIX
        // pthread_create), and so does a vfork-style child (fork semantics).
        // Dispositions live on the thread-group leader, so a CLONE_THREAD
        // sibling's own table is never consulted and stays at the default;
        // a non-CLONE_THREAD child is a new process and copies them like
        // fork_current does.
        child.signal_mask = signal_mask;
        if flags & CLONE_THREAD == 0 {
            inherit_signal_actions(&mut child, parent_tgid);
        } else {
            child.signal_actions = [DEFAULT_SIGACTION; 64];
        }
        child.vfork_pending = flags & CLONE_VFORK != 0;
        if flags & CLONE_CHILD_CLEARTID != 0 {
            child.clear_child_tid = ctid;
        }

        // Give the caller its chance to set up per-child kernel state (VFS
        // fd table) while the child is still invisible to other CPUs — same
        // SMP race fork_current's doc comment describes: another CPU could
        // otherwise dispatch the child immediately and lose the race against
        // its own first fd-allocating syscall.
        // A non-CLONE_THREAD clone is a new process (own tgid) — inherit the
        // parent's /proc/self/exe path; CLONE_THREAD siblings share it by tgid.
        if flags & CLONE_THREAD == 0 {
            super::inherit_exe_path(parent_tgid, child_pid);
        }
        before_enqueue(child_pid);

        if !super::RUN_QUEUE.lock().enqueue(child) {
            report_task_table_full();
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
            // Park until the child execs or exits (`vfork_complete` / the exit
            // paths wake VFORK_WAIT_CHANNEL). Three-phase so a release that
            // lands between the check and the park is not lost. Signals do not
            // interrupt this wait — Linux holds them too while the child borrows
            // the address space.
            let pending = |child_pid| {
                let rq = super::RUN_QUEUE.lock();
                match rq.find_pid(child_pid) {
                    Some(t) => t.vfork_pending,
                    None    => false, // already reaped — definitely done
                }
            };
            while pending(child_pid) {
                super::block_on_port_prepare(super::VFORK_WAIT_CHANNEL);
                if !pending(child_pid) { super::block_on_port_cancel(); break; }
                super::block_on_port_commit();
            }
        }

        child_pid as isize
    }
}
