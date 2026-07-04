//! Task (process/thread) descriptor — analogous to Linux `task_struct`.

extern crate alloc;

use crate::context::CpuContext;
use mm::vmm::AddressSpace;

pub type Pid = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,  // Waiting on an IPC port or futex.
    Zombie,
}

// ── EEVDF scheduling parameters ──────────────────────────────────────────────
//
// The run queue picks tasks with an Earliest-Eligible-Virtual-Deadline-First
// policy (see runqueue.rs).  Each task carries a `weight` derived from its
// nice value (`priority`), a `vruntime` that advances by
// `ticks × NICE0_WEIGHT / weight` while it runs, and a `vdeadline` renewed to
// `vruntime + BASE_SLICE_TICKS × NICE0_WEIGHT / weight` whenever it expires.

/// Weight of a nice-0 task; vruntime is scaled so a nice-0 task accrues one
/// unit of virtual time per timer tick.
pub const NICE0_WEIGHT: u64 = 1024;

/// Nominal slice, in 100 Hz timer ticks, granted per deadline period.
pub const BASE_SLICE_TICKS: u64 = 4;

/// Linux's `sched_prio_to_weight[]` — index by `nice + 20`.  Each step of one
/// nice level changes the CPU share by ~1.25×.
const NICE_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291,
    29154, 23254, 18705, 14949, 11916,
     9548,  7620,  6100,  4904,  3906,
     3121,  2501,  1991,  1586,  1277,
     1024,   820,   655,   526,   423,
      335,   272,   215,   172,   137,
      110,    87,    70,    56,    45,
       36,    29,    23,    18,    15,
];

/// Map a nice value (clamped to −20..19) to a load weight.
pub fn nice_to_weight(nice: i8) -> u32 {
    let idx = (nice.clamp(-20, 19) + 20) as usize;
    NICE_TO_WEIGHT[idx]
}

/// Per-signal disposition. `sys_sigaction` (`sched/src/signal.rs`) reads/
/// writes this directly via `core::ptr::read`/`write` against whatever the
/// caller passed as `struct sigaction*`, so the field order here must match
/// the real POSIX/relibc layout (`sa_handler, sa_flags, sa_restorer,
/// sa_mask` — see `userland/relibc/src/header/signal/mod.rs`) byte-for-byte,
/// not just by name.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SigAction {
    /// Handler address: 0 = SIG_DFL, 1 = SIG_IGN, else a user-space fn ptr.
    pub handler:  usize,
    pub flags:    u32,
    /// `sa_restorer` — user-space trampoline that calls `sys_rt_sigreturn`.
    /// 0 = use the kernel's built-in trampoline page.
    pub restorer: usize,
    /// Signal mask to apply during handler execution.
    pub mask:     u64,
}

pub const DEFAULT_SIGACTION: SigAction =
    SigAction { handler: 0, flags: 0, mask: 0, restorer: 0 };

#[repr(C)]
pub struct Task {
    pub pid:          Pid,
    pub state:        TaskState,
    pub priority:     i8,
    /// CPU currently executing this task (`None` = not on any CPU).
    ///
    /// SMP guard: `pick_next` skips tasks with `on_cpu.is_some()` so a task
    /// whose registers are still being saved on one core can never be picked
    /// up by another.  Set by the scheduler when dispatching; cleared in the
    /// scheduler context after `cpu_switch_to` returns.
    pub on_cpu:       Option<usize>,
    /// EEVDF load weight, derived from `priority` via [`nice_to_weight`].
    pub weight:       u32,
    /// EEVDF weighted virtual runtime (NICE0-tick units).
    pub vruntime:     u64,
    /// EEVDF virtual deadline; the runnable, eligible task with the earliest
    /// deadline is picked next.
    pub vdeadline:    u64,
    /// Saved CPU register state.
    pub ctx:          CpuContext,
    /// Root page table physical address (0 = use kernel tables).
    pub page_table:   usize,
    /// Physical address of the bottom of this task's kernel stack allocation.
    pub kernel_stack: usize,
    /// IPC port this task is sleeping on (Some when state == Blocked on IPC).
    pub blocked_on:   Option<u32>,
    /// Futex user-space address this task is waiting on (0 = none).
    pub blocked_futex: usize,
    /// Per-process virtual address space (None for kernel tasks).
    pub address_space: Option<alloc::boxed::Box<AddressSpace>>,
    /// Exit status set by `exit()`.  Valid only when `state == Zombie`.
    pub exit_code:    i32,
    /// Dedicated reply port for sys_call.  Allocated at spawn; freed on exit.
    /// `u32::MAX` = not yet allocated.
    pub reply_port:   u32,

    // ── POSIX process identity ────────────────────────────────────────────────
    pub ppid: Pid,   // parent PID
    pub tgid: Pid,   // thread group leader PID (== pid for single-threaded tasks)
    pub pgid: Pid,   // process group ID
    pub sid:  Pid,   // session ID

    // ── POSIX credentials ─────────────────────────────────────────────────────
    pub uid:  u32,
    pub gid:  u32,
    pub euid: u32,
    pub egid: u32,

    // ── Signal state ──────────────────────────────────────────────────────────
    /// Bitmask of pending signals (bit N = signal N+1 is pending).
    pub signal_pending: u64,
    /// Bitmask of blocked (masked) signals.
    pub signal_mask:    u64,
    /// Per-signal disposition table (reduced for testing).
    pub signal_actions: [SigAction; 64],

    // ── Thread state ──────────────────────────────────────────────────────────
    /// User-space address of the thread's TID word (for `set_tid_address`).
    /// Written to 0 and futex-woken on thread exit so `pthread_join` works.
    pub clear_child_tid: usize,

    // ── Heap bookmarks (for sys_brk) ─────────────────────────────────────────
    pub heap_start: usize,
    pub heap_end:   usize,

    // ── Architecture-specific TLS register ───────────────────────────────────
    /// x86-64: FS.base (thread-local storage pointer), saved/restored on switch.
    /// AArch64: TPIDR_EL0, saved/restored on switch.
    pub tls_base: u64,

    // ── Filesystem state ──────────────────────────────────────────────────────
    /// Current working directory (fixed-size buffer for Phase 1).
    pub cwd:     [u8; 128],
    pub cwd_len: usize,
    /// File-creation mask (POSIX umask).
    pub umask:   u32,

    // ── Alternate signal stack (sigaltstack) ──────────────────────────────────
    /// User-space base of the alternate signal stack; 0 if none configured.
    pub altstack_sp:    usize,
    /// Size in bytes of the alternate signal stack.
    pub altstack_size:  usize,
    /// `SS_DISABLE` (2) if no stack is configured (the default), else 0.
    /// `SS_ONSTACK` is never stored here — it's derived from the live user
    /// SP at query time, matching Linux's `on_sig_stack()`.
    pub altstack_flags: u32,
}

impl Task {
    /// Virtual-time length of one slice for a task of `weight`.
    pub fn slice_vt(weight: u32) -> u64 {
        (BASE_SLICE_TICKS * NICE0_WEIGHT / weight as u64).max(1)
    }

    /// Charge `delta_ticks` of CPU time against this task's virtual runtime
    /// and renew the virtual deadline once it is used up.
    ///
    /// A minimum of one virtual-time unit is charged per dispatch so that
    /// tasks yielding faster than the timer tick still make forward progress
    /// in virtual time and cannot starve CPU-bound tasks.
    pub fn charge_vruntime(&mut self, delta_ticks: u64) {
        let charged = (delta_ticks * NICE0_WEIGHT / self.weight as u64).max(1);
        self.vruntime += charged;
        if self.vruntime >= self.vdeadline {
            self.vdeadline = self.vruntime + Self::slice_vt(self.weight);
        }
    }

    /// (Re-)place this task among the runnable set: clamp `vruntime` to the
    /// queue's minimum so a long sleep doesn't turn into a burst of unfair
    /// CPU time on wake-up (EEVDF lag limiting), and grant a fresh deadline.
    pub fn place(&mut self, min_vruntime: u64) {
        if self.vruntime < min_vruntime {
            self.vruntime = min_vruntime;
        }
        self.vdeadline = self.vruntime + Self::slice_vt(self.weight);
    }

    /// Create a kernel-mode task that starts at `entry`.
    pub fn new_kernel(
        pid:        Pid,
        entry:      usize,
        stack_base: usize,
        stack_size: usize,
        page_table: usize,
    ) -> alloc::boxed::Box<Self> {
        extern "C" { fn arch_serial_putc(b: u8); }
        let msg_direct = b"Task::new_kernel: using clean Box::new allocation\r\n";
        for &b in msg_direct { unsafe { arch_serial_putc(b); } }

        // Create task struct directly using Box::new for clean, single allocation
        let mut temp_task = Task {
            pid,
            state: TaskState::Ready,
            priority: 0,
            on_cpu: None,
            weight: nice_to_weight(0),
            vruntime: 0,
            vdeadline: 0,
            ctx: if entry == 0 {
                CpuContext::zeroed()
            } else {
                 CpuContext::new_task(entry, mm::phys_to_virt(stack_base) + stack_size)
            },
            page_table,
            kernel_stack: stack_base,
            blocked_on: None,
            blocked_futex: 0,
            address_space: None,
            exit_code: 0,
            reply_port: u32::MAX,
            ppid: 0,
            tgid: pid,
            pgid: pid,
            sid: pid,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            signal_pending: 0,
            signal_mask: 0,
            signal_actions: [DEFAULT_SIGACTION; 64],
            clear_child_tid: 0,
            heap_start: 0,
            heap_end: 0,
            tls_base: 0,
            cwd: [0; 128],
            cwd_len: 1, // Default to "/"
            umask: 0o022,
            altstack_sp: 0,
            altstack_size: 0,
            altstack_flags: 2, // SS_DISABLE
        };
        temp_task.cwd[0] = b'/';

        let msg_done = b"Task::new_kernel: task ready with clean allocation\r\n";
        for &b in msg_done { unsafe { arch_serial_putc(b); } }

        // Move to heap using Box::new (clean, single allocation)
        alloc::boxed::Box::new(temp_task)
    }


    /// Create a kernel-mode task directly in the provided memory location.
    /// This avoids large struct moves that can cause stack overflows.
    pub unsafe fn new_kernel_inplace(
        dest: *mut Self,
        pid: Pid,
        entry: usize,
        stack_base: usize,
        stack_size: usize,
        page_table: usize,
    ) {
        extern "C" { fn arch_serial_putc(b: u8); }
        let msg1 = b"Task::new_kernel_inplace: starting\r\n";
        for &b in msg1 { arch_serial_putc(b); }

        let stack_top = mm::phys_to_virt(stack_base) + stack_size;

        let msg2 = b"Task::new_kernel_inplace: about to write pid to addr=";
        for &b in msg2 { arch_serial_putc(b); }

        // Print the destination address
        let dest_addr = dest as usize;
        for i in (0..8).rev() {
            let digit = ((dest_addr >> (i * 4)) & 0xF) as u8;
            let c = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            arch_serial_putc(c);
        }
        arch_serial_putc(b'\r');
        arch_serial_putc(b'\n');

        // Test if we can even read from this address
        let test_msg = b"Task::new_kernel_inplace: testing read access\r\n";
        for &b in test_msg { arch_serial_putc(b); }

        let _test_byte = unsafe { core::ptr::read_volatile(dest as *const u8) };

        let success_msg = b"Task::new_kernel_inplace: read succeeded, about to write pid\r\n";
        for &b in success_msg { arch_serial_putc(b); }

        // Try a simple byte write first
        let test_write_msg = b"Task::new_kernel_inplace: testing simple byte write\r\n";
        for &b in test_write_msg { arch_serial_putc(b); }

        core::ptr::write_volatile(dest as *mut u8, 0xAB);

        let byte_write_ok = b"Task::new_kernel_inplace: byte write succeeded\r\n";
        for &b in byte_write_ok { arch_serial_putc(b); }

        // Check field offset and try writing to exact position
        let offset_msg = b"Task::new_kernel_inplace: checking pid field offset\r\n";
        for &b in offset_msg { arch_serial_putc(b); }

        let pid_offset = core::mem::offset_of!(Task, pid);
        let pid_addr = (dest as usize) + pid_offset;

        let addr_msg = b"PID field at offset=";
        for &b in addr_msg { arch_serial_putc(b); }
        for i in (0..4).rev() {
            let digit = ((pid_offset >> (i * 4)) & 0xF) as u8;
            let c = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            arch_serial_putc(c);
        }
        arch_serial_putc(b' ');
        for i in (0..8).rev() {
            let digit = ((pid_addr >> (i * 4)) & 0xF) as u8;
            let c = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            arch_serial_putc(c);
        }
        arch_serial_putc(b'\r');
        arch_serial_putc(b'\n');

        // Debug memory attributes for the allocated address
        let debug_msg = b"Task::new_kernel_inplace: calling memory debug\r\n";
        for &b in debug_msg { arch_serial_putc(b); }

        // DISABLED: This was causing race conditions in the page table walking code
        // extern "C" {
        //     fn debug_memory_attributes_aarch64(addr: usize);
        // }
        // debug_memory_attributes_aarch64(dest as usize);

        // Initialize critical fields
        let init_msg = b"Task::new_kernel_inplace: initializing fields\r\n";
        for &b in init_msg { arch_serial_putc(b); }

        // Test different approaches to field access
        let test_approaches_msg = b"Task::new_kernel_inplace: testing different access patterns\r\n";
        for &b in test_approaches_msg { arch_serial_putc(b); }

        // Approach 1: Try direct field assignment via volatile operations
        let approach1_msg = b"Approach 1: Direct volatile write\r\n";
        for &b in approach1_msg { arch_serial_putc(b); }

        let pid_ptr = (dest as usize + core::mem::offset_of!(Task, pid)) as *mut Pid;
        core::ptr::write_volatile(pid_ptr, pid);

        let success1_msg = b"Approach 1: PID write succeeded\r\n";
        for &b in success1_msg { arch_serial_putc(b); }

        // Try reading it back
        let read_pid = core::ptr::read_volatile(pid_ptr);
        if read_pid == pid {
            let verify_msg = b"Approach 1: PID verification succeeded\r\n";
            for &b in verify_msg { arch_serial_putc(b); }
        } else {
            let verify_fail_msg = b"Approach 1: PID verification FAILED\r\n";
            for &b in verify_fail_msg { arch_serial_putc(b); }
        }

        // Continue with other critical fields using the same approach
        let state_ptr = (dest as usize + core::mem::offset_of!(Task, state)) as *mut TaskState;
        core::ptr::write_volatile(state_ptr, TaskState::Ready);

        let priority_ptr = (dest as usize + core::mem::offset_of!(Task, priority)) as *mut i8;
        core::ptr::write_volatile(priority_ptr, 0);

        let on_cpu_ptr = (dest as usize + core::mem::offset_of!(Task, on_cpu)) as *mut Option<usize>;
        core::ptr::write_volatile(on_cpu_ptr, None);

        let weight_ptr = (dest as usize + core::mem::offset_of!(Task, weight)) as *mut u32;
        core::ptr::write_volatile(weight_ptr, nice_to_weight(0));

        let vruntime_ptr = (dest as usize + core::mem::offset_of!(Task, vruntime)) as *mut u64;
        core::ptr::write_volatile(vruntime_ptr, 0);

        let vdeadline_ptr = (dest as usize + core::mem::offset_of!(Task, vdeadline)) as *mut u64;
        core::ptr::write_volatile(vdeadline_ptr, 0);

        // Create the CPU context
        let ctx_msg = b"Task::new_kernel_inplace: creating CpuContext\r\n";
        for &b in ctx_msg { arch_serial_putc(b); }

        // Create context step by step to debug the FP/SIMD issue
        let debug_ctx_msg = b"Task::new_kernel_inplace: creating context step by step\r\n";
        for &b in debug_ctx_msg { arch_serial_putc(b); }

        let ctx_ptr = (dest as usize + core::mem::offset_of!(Task, ctx)) as *mut CpuContext;

        // Initialize the context fields directly to avoid any potential FP/SIMD issues
        unsafe {
            // Zero the entire context first
            core::ptr::write_bytes(ctx_ptr as *mut u8, 0, core::mem::size_of::<CpuContext>());

            let step1_msg = b"Step 1: Zeroed context\r\n";
            for &b in step1_msg { arch_serial_putc(b); }

            // Set up the basic registers without touching FP/SIMD
            let gregs_ptr = ctx_ptr as *mut [u64; 12];
            let mut gregs = [0u64; 12];
            gregs[11] = entry as u64;  // x30 (lr) = entry point
            core::ptr::write_volatile(gregs_ptr, gregs);

            let step2_msg = b"Step 2: Set general purpose registers\r\n";
            for &b in step2_msg { arch_serial_putc(b); }

            // Set stack pointer
            #[cfg(target_arch = "aarch64")]
            let sp_ptr = (ctx_ptr as usize + core::mem::offset_of!(CpuContext, sp)) as *mut u64;
            #[cfg(not(target_arch = "aarch64"))]
            let sp_ptr = (ctx_ptr as usize + core::mem::offset_of!(CpuContext, rsp)) as *mut u64;
            core::ptr::write_volatile(sp_ptr, stack_top as u64);

            let step3_msg = b"Step 3: Set stack pointer\r\n";
            for &b in step3_msg { arch_serial_putc(b); }

            let complete_msg = b"Task::new_kernel_inplace: context creation complete\r\n";
            for &b in complete_msg { arch_serial_putc(b); }
        }

        // Convert all remaining field writes to direct volatile operations
        let page_table_ptr = (dest as usize + core::mem::offset_of!(Task, page_table)) as *mut usize;
        core::ptr::write_volatile(page_table_ptr, page_table);

        let kernel_stack_ptr = (dest as usize + core::mem::offset_of!(Task, kernel_stack)) as *mut usize;
        core::ptr::write_volatile(kernel_stack_ptr, stack_base);

        let blocked_on_ptr = (dest as usize + core::mem::offset_of!(Task, blocked_on)) as *mut Option<u32>;
        core::ptr::write_volatile(blocked_on_ptr, None);

        let blocked_futex_ptr = (dest as usize + core::mem::offset_of!(Task, blocked_futex)) as *mut usize;
        core::ptr::write_volatile(blocked_futex_ptr, 0);

        let address_space_ptr = (dest as usize + core::mem::offset_of!(Task, address_space)) as *mut Option<AddressSpace>;
        core::ptr::write_volatile(address_space_ptr, None);

        let exit_code_ptr = (dest as usize + core::mem::offset_of!(Task, exit_code)) as *mut i32;
        core::ptr::write_volatile(exit_code_ptr, 0);

        let reply_port_ptr = (dest as usize + core::mem::offset_of!(Task, reply_port)) as *mut u32;
        core::ptr::write_volatile(reply_port_ptr, u32::MAX);

        let ppid_ptr = (dest as usize + core::mem::offset_of!(Task, ppid)) as *mut Pid;
        core::ptr::write_volatile(ppid_ptr, 0);

        let tgid_ptr = (dest as usize + core::mem::offset_of!(Task, tgid)) as *mut Pid;
        core::ptr::write_volatile(tgid_ptr, pid);

        let pgid_ptr = (dest as usize + core::mem::offset_of!(Task, pgid)) as *mut Pid;
        core::ptr::write_volatile(pgid_ptr, pid);

        let sid_ptr = (dest as usize + core::mem::offset_of!(Task, sid)) as *mut Pid;
        core::ptr::write_volatile(sid_ptr, pid);

        let uid_ptr = (dest as usize + core::mem::offset_of!(Task, uid)) as *mut u32;
        core::ptr::write_volatile(uid_ptr, 0);

        let gid_ptr = (dest as usize + core::mem::offset_of!(Task, gid)) as *mut u32;
        core::ptr::write_volatile(gid_ptr, 0);

        let euid_ptr = (dest as usize + core::mem::offset_of!(Task, euid)) as *mut u32;
        core::ptr::write_volatile(euid_ptr, 0);

        let egid_ptr = (dest as usize + core::mem::offset_of!(Task, egid)) as *mut u32;
        core::ptr::write_volatile(egid_ptr, 0);

        let signal_pending_ptr = (dest as usize + core::mem::offset_of!(Task, signal_pending)) as *mut u64;
        core::ptr::write_volatile(signal_pending_ptr, 0);

        let signal_mask_ptr = (dest as usize + core::mem::offset_of!(Task, signal_mask)) as *mut u64;
        core::ptr::write_volatile(signal_mask_ptr, 0);

        let clear_child_tid_ptr = (dest as usize + core::mem::offset_of!(Task, clear_child_tid)) as *mut usize;
        core::ptr::write_volatile(clear_child_tid_ptr, 0);

        let heap_start_ptr = (dest as usize + core::mem::offset_of!(Task, heap_start)) as *mut usize;
        core::ptr::write_volatile(heap_start_ptr, 0);

        let heap_end_ptr = (dest as usize + core::mem::offset_of!(Task, heap_end)) as *mut usize;
        core::ptr::write_volatile(heap_end_ptr, 0);

        let tls_base_ptr = (dest as usize + core::mem::offset_of!(Task, tls_base)) as *mut u64;
        core::ptr::write_volatile(tls_base_ptr, 0);

        let cwd_len_ptr = (dest as usize + core::mem::offset_of!(Task, cwd_len)) as *mut usize;
        core::ptr::write_volatile(cwd_len_ptr, 1);

        let umask_ptr = (dest as usize + core::mem::offset_of!(Task, umask)) as *mut u32;
        core::ptr::write_volatile(umask_ptr, 0o022);

        let altstack_sp_ptr = (dest as usize + core::mem::offset_of!(Task, altstack_sp)) as *mut usize;
        core::ptr::write_volatile(altstack_sp_ptr, 0);

        let altstack_size_ptr = (dest as usize + core::mem::offset_of!(Task, altstack_size)) as *mut usize;
        core::ptr::write_volatile(altstack_size_ptr, 0);

        let altstack_flags_ptr = (dest as usize + core::mem::offset_of!(Task, altstack_flags)) as *mut u32;
        core::ptr::write_volatile(altstack_flags_ptr, 2); // SS_DISABLE

        // Initialize signal_actions array with DEFAULT_SIGACTION
        let signal_actions_ptr = (dest as usize + core::mem::offset_of!(Task, signal_actions)) as *mut [SigAction; 64];
        for i in 0..64 {
            let action_ptr = (signal_actions_ptr as usize + i * core::mem::size_of::<SigAction>()) as *mut SigAction;
            core::ptr::write_volatile(action_ptr, DEFAULT_SIGACTION);
        }

        // Initialize cwd array to all zeros, then set first byte to '/'
        let cwd_ptr = (dest as usize + core::mem::offset_of!(Task, cwd)) as *mut [u8; 128];
        core::ptr::write_bytes(cwd_ptr as *mut u8, 0, 128);
        let cwd_first_ptr = cwd_ptr as *mut u8;
        core::ptr::write_volatile(cwd_first_ptr, b'/');
        
        let cwd_len_ptr = (dest as usize + core::mem::offset_of!(Task, cwd_len)) as *mut usize;
        core::ptr::write_volatile(cwd_len_ptr, 1);

        let msg2 = b"Task::new_kernel_inplace: completed\r\n";
        for &b in msg2 { arch_serial_putc(b); }
    }

    /// Create a userspace task that transitions directly to userspace
    pub fn new_userspace(
        pid: Pid,
        user_entry: usize,
        user_sp: usize,
        kernel_stack_phys: usize,
        kernel_stack_size: usize,
        page_table: usize,
    ) -> alloc::boxed::Box<Self> {
        extern "C" { fn arch_serial_putc(b: u8); }
        let debug_msg = b"Task::new_userspace: creating userspace task\r\n";
        for &b in debug_msg { unsafe { arch_serial_putc(b); } }

        let kernel_stack_virt = mm::phys_to_virt(kernel_stack_phys);

        let mut task = Task {
            pid,
            state: TaskState::Ready,
            priority: 0,
            on_cpu: None,
            weight: nice_to_weight(0),
            vruntime: 0,
            vdeadline: 0,
            ctx: crate::context::CpuContext::new_user_task_with_pt(user_entry, user_sp, kernel_stack_virt + kernel_stack_size, page_table),
            page_table,

            kernel_stack: kernel_stack_phys,
            blocked_on: None,
            blocked_futex: 0,
            address_space: None,
            exit_code: 0,
            reply_port: u32::MAX,
            ppid: 0,
            tgid: pid,
            pgid: pid,
            sid: pid,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            signal_pending: 0,
            signal_mask: 0,
            signal_actions: [DEFAULT_SIGACTION; 64],
            clear_child_tid: 0,
            heap_start: 0,
            heap_end: 0,
            tls_base: 0,
            cwd: [0; 128],
            cwd_len: 1, // Default to "/"
            umask: 0o022,
            altstack_sp: 0,
            altstack_size: 0,
            altstack_flags: 2, // SS_DISABLE
        };
        task.cwd[0] = b'/';

        let success_msg = b"Task::new_userspace: userspace task created successfully\r\n";
        for &b in success_msg { unsafe { arch_serial_putc(b); } }

        alloc::boxed::Box::new(task)
    }

    /// Create a minimal test task using unsafe initialization to avoid stack issues.
    /// This proves the scheduler core functionality works.
    pub fn new_minimal_test(pid: Pid, entry: usize, stack_base: usize, stack_size: usize) -> Self {
        extern "C" { fn arch_serial_putc(b: u8); }
        let msg1 = b"Task::new_minimal_test: using unsafe init\r\n";
        for &b in msg1 { unsafe { arch_serial_putc(b); } }

        // Debug: print new Task struct size
        let task_size = core::mem::size_of::<Task>();
        let msg_debug = b"Task size now: ";
        for &b in msg_debug { unsafe { arch_serial_putc(b); } }
        let n = task_size;
        for i in (0..8).rev() {
            let digit = ((n >> (i * 4)) & 0xF) as u8;
            let c = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            unsafe { arch_serial_putc(c); }
        }
        let msg_end = b"\r\n";
        for &b in msg_end { unsafe { arch_serial_putc(b); } }

        let _stack_top = stack_base + stack_size;

        // Use the buddy allocator approach since it was working correctly
        // when the memory regions were fixed
        unsafe {
            let task_size = core::mem::size_of::<Task>();
            let page_size = mm::buddy::PAGE_SIZE;

            let order = {
                let mut o = 0;
                let mut size = page_size;
                while size < task_size {
                    size *= 2;
                    o += 1;
                }
                o
            };

            let ptr = match mm::buddy::alloc(order) {
                Some(addr) => addr as *mut Task,
                None => panic!("Failed to allocate Task struct"),
            };

            // Zero the memory
            core::ptr::write_bytes(ptr as *mut u8, 0, task_size);

            // Initialize in-place
            Self::new_kernel_inplace(ptr, pid, entry, stack_base, stack_size, 0);

            // Copy to stack and free buddy allocation to return a proper Task
            let task_ref = &*ptr;
            let task = core::ptr::read(task_ref);
            mm::buddy::free(ptr as usize, order);

            task
        }
    }
}
