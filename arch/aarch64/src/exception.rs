//! AArch64 exception vector table and handlers.
//!
//! The table must be 2 KiB aligned (VBAR_EL1 requirement).
//! Each of the 16 vector slots is 128 bytes; we branch to out-of-line
//! handlers so the slots only hold a single `b handler`.
//!
//! Spec: ARMv8-A Architecture Reference Manual §D1.10 (Exception Handling)

core::arch::global_asm!(include_str!("exception_asm.s"));

extern "C" {
    fn serial_print(ptr: *const u8, len: usize);
    fn print_hex(n: usize);
    fn print_number(n: u32);
}

fn serial_print_str(s: &str) {
    unsafe { serial_print(s.as_ptr(), s.len()); }
}

#[allow(dead_code)]
extern "C" {
    /// Save all registers and call the appropriate Rust handler.
    fn exc_vector_table();
}

/// Initialize the exception vector table by setting `VBAR_EL1`.
pub fn init() {
    unsafe {
        let table_addr = exc_vector_table_ptr();
        core::arch::asm!("msr vbar_el1, {}", in(reg) table_addr);
    }
}

fn exc_vector_table_ptr() -> usize {
    extern "C" {
        static __exception_vectors: u8;
    }
    core::ptr::addr_of!(__exception_vectors) as usize
}

/// Updates the per-CPU kernel stack pointer used on EL0 exception entry.
#[no_mangle]
pub unsafe extern "C" fn arch_set_kernel_stack(kst: u64) {
    core::arch::asm!("msr tpidr_el1, {}", in(reg) kst);
}

// ── Exception Frame ──────────────────────────────────────────────────────────

// Use the definition from sched::context to ensure consistency
pub use sched::context::UserFrame;

// ── Sync Exception Handlers ──────────────────────────────────────────────────

fn handle_irq(_frame: *mut UserFrame) {
    let iar = super::gic::ack();
    let irq_id = super::gic::irq_id(iar);

    if irq_id == 27 || irq_id == 30 {
        // Virtual or Physical Timer
        super::timer::on_tick();
    } else if irq_id == super::gic::SGI_RESCHED {
        // Reschedule IPI from another CPU.  The sender already set this
        // CPU's PREEMPT_NEEDED flag; the preempt_check below acts on it.
        // An idle CPU parked in wfi is woken by the interrupt itself.
    } else if irq_id == 33 {
        // PL011 UART
        while let Some(b) = unsafe { super::uart::getc() } {
            // Line-discipline ISIG intercept: ^C/^\/^Z become signals to
            // the foreground process group instead of input bytes.
            if tty_server::console_intercept_byte(b) { continue; }
            evdev_server::push_event(0, 1 /* EV_KEY */, b as u16, 2);
            evdev_server::push_event(0, 0 /* EV_SYN */, 0 /* SYN_REPORT */, 0);
        }
        unsafe { super::uart::clear_irq(); }
    } else if irq_id != super::gic::SPURIOUS {
        serial_print_str("\n[EXC] Unhandled IRQ ");
        unsafe { print_number(irq_id); }
        serial_print_str("\n");
    }

    super::gic::eoi(iar);
    sched::preempt_check();
}

#[no_mangle]
unsafe extern "C" fn exc_el1_irq_handler(frame: *mut UserFrame) {
    handle_irq(frame);
}

#[no_mangle]
unsafe extern "C" fn exc_el0_irq_handler(frame: *mut UserFrame) {
    handle_irq(frame);
}

#[no_mangle]
unsafe extern "C" fn exc_el1_sync_handler(esr: u64, elr: u64) {
    let far: u64;
    let tcr: u64;
    core::arch::asm!("mrs {}, far_el1", out(reg) far);
    core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr);

    // Kernel-mode data abort on a *user* (TTBR0) address: kernel/server code
    // dereferenced a user pointer whose page is demand-paged (lazy heap, CoW,
    // or a never-touched page of a file-backed exec image).  The servers run
    // synchronously in the calling task's context, so the current task's
    // address space is the right one — service it like a user fault; on
    // success the vector stub erets back to the faulting kernel instruction.
    //
    // Deadlock note: faults needing a *file read* must never be taken while
    // filesystem locks are held; the syscall layer prefaults every user
    // buffer it forwards into VFS.  This path is the safety net for the rest.
    //
    //   EC 0x25 = data abort from the current EL (kernel mode).
    //   DFSC 0x04..=0x07 translation fault, 0x0D..=0x0F permission fault.
    let ec   = (esr >> 26) & 0x3F;
    let dfsc = esr & 0x3F;
    if ec == 0x25 && far >> 48 == 0 {
        let is_translation = (0x04..=0x07).contains(&dfsc);
        let is_permission  = (0x0D..=0x0F).contains(&dfsc);
        let is_write = (esr >> 6) & 1 != 0;
        if (is_translation || is_permission)
            && sched::handle_page_fault(far as usize, is_write)
        {
            return; // fault serviced — resume the interrupted kernel code
        }
    }

    serial_print_str("\n[EXC] EL1 Sync Fault! ESR=");
    print_hex(esr as usize);
    serial_print_str(" ELR=");
    print_hex(elr as usize);
    serial_print_str(" FAR=");
    print_hex(far as usize);
    serial_print_str(" TCR=");
    print_hex(tcr as usize);
    serial_print_str("\n");

    // A kernel-mode data abort (EC 0x25) on a *user* (TTBR0, FAR[63:48]==0)
    // address that handle_page_fault could not service means the CURRENT task
    // handed the kernel a bad/unmapped user pointer inside a syscall (e.g. a
    // stray argv/envp pointer during execve).  Kill that task — a
    // SIGSEGV-equivalent exit — instead of hanging the whole machine.  A
    // genuine kernel bug (a fault on a kernel TTBR1 address, or an instruction
    // abort at EL1) is not the task's fault and still halts here for triage.
    if ec == 0x25 && far >> 48 == 0 {
        serial_print_str("[EXC] killing PID=");
        print_number(sched::current_pid());
        serial_print_str(" (unresolvable user-pointer fault in kernel)\n");
        sched::exit(1);
    }

    loop { core::hint::spin_loop(); }
}

#[no_mangle]
unsafe extern "C" fn exc_el0_sync_handler(esr: u64, elr: u64, frame: *mut UserFrame) {
    let ec = (esr >> 26) & 0x3F;
    if ec == 0x15 {
        serial_print_str("[EXC] Unexpected syscall in Rust handler\n");
    } else {
        let far: u64;
        core::arch::asm!("mrs {}, far_el1", out(reg) far);

        // Demand paging: a translation fault on a lazily-mapped region (BSS,
        // growable heap/stack) just means the page has not been backed yet.
        // Fault it in and return — the vector's `ret_to_user` path then `eret`s
        // back to ELR_EL1, retrying the faulting instruction.  This mirrors the
        // x86_64 #PF handler, which the shared ELF loader relies on to zero-fill
        // BSS pages where `p_memsz > p_filesz`.
        //
        //   EC 0x24 = data abort, 0x20 = instruction abort, both from a lower EL.
        //   DFSC 0x04..=0x07 = translation fault (levels 0–3 ⇒ page not present).
        //   DFSC 0x0D..=0x0F = permission fault (levels 1–3 ⇒ present but
        //   disallowed) — also routed here so a write to a read-only CoW
        //   page can be promoted instead of killing the task outright.
        //   WnR (ISS bit 6) only applies to data aborts; an instruction
        //   abort is a fetch, never a CoW write.
        let dfsc = esr & 0x3F;
        if ec == 0x24 || ec == 0x20 {
            let is_translation = (0x04..=0x07).contains(&dfsc);
            let is_permission  = (0x0D..=0x0F).contains(&dfsc);
            let is_write = ec == 0x24 && (esr >> 6) & 1 != 0;
            if (is_translation || is_permission) && sched::handle_page_fault(far as usize, is_write) {
                return; // page mapped — resume EL0 and retry the faulting access
            }
            
            serial_print_str("[FAULT] far=");
            print_hex(far as usize);
            serial_print_str(" dfsc=");
            print_hex(dfsc as usize);
            serial_print_str(" elr=");
            print_hex(elr as usize);
            serial_print_str("\n");
        }

        serial_print_str("\n[EXC] EL0 Fault! PID=");
        print_number(sched::current_pid());
        serial_print_str(" ESR=");
        print_hex(esr as usize);
        serial_print_str(" FAR=");
        print_hex(far as usize);
        serial_print_str(" EC=");
        print_hex(ec as usize);
        serial_print_str(" DFSC=");
        print_hex((esr & 0x3F) as usize);
        serial_print_str(" ELR=");
        print_hex(elr as usize);
        
        // Print instruction at ELR if possible
        if elr >= 0x200000 && elr < 0x80000000 {
            // We need to be in the same address space to read this!
            // But wait! We are in EL1, we can't easily read TTBR0 memory if it's not mapped in EL1.
            // However, we are identity mapped for the first 1GB in some boot paths.
            // For now, let's just print the ESR/ELR and try to deduce.
            let _instr_ptr = elr as *const u32;
        }
        
        // Print some regs from frame
        serial_print_str("\n[EXC] x0=");
        print_hex((*frame).x[0] as usize);
        serial_print_str(" x1=");
        print_hex((*frame).x[1] as usize);
        serial_print_str(" sp=");
        print_hex((*frame).sp_el0 as usize);
        serial_print_str("\n");

        // ── EL0 fault-time user backtrace (gated diagnostic) ─────────────────
        // Walks the AArch64 frame-pointer (x29) chain to name an unbounded
        // userspace recursion whose only clue is a write-fault at sp driven to
        // the main-stack base. (This cracked W3: a signal_hook_registry handler
        // chaining to itself in brush after execve failed to reset a caught
        // disposition — fixed by sched::signal::reset_handlers_on_exec.)
        // AArch64 prologues save the caller's fp/lr as
        // `stp x29,x30,[sp,#-N]!` + `add x29,sp,#k`, so at any frame
        // `*(fp) = caller_fp` and `*(fp+8) = return_addr`. Every read is bounded
        // to the main thread's eagerly-mapped 8 MiB user stack window (mirrors
        // kernel/src/syscall.rs USER_STACK_TOP/USER_STACK_SIZE) so a read cannot
        // fault-in-fault and re-enter this EL1 abort handler → deadlock. A worker
        // thread's fp (musl mmap stack) falls outside the window ⇒ the walk is a
        // safe no-op there. Symbolize offline:
        //   llvm-addr2line -f -i -C -e cosmic-comp-<arch> <ret − 0x200000>
        const EL0_BACKTRACE: bool = false;
        if EL0_BACKTRACE {
            const STACK_TOP: usize = 0x0000_7fff_ffff_f000;
            const STACK_SIZE: usize = 2048 * 4096; // 8 MiB, USER_STACK_SIZE
            const STACK_BASE: usize = STACK_TOP - STACK_SIZE;
            serial_print_str("[BT] base=0x200000 elr=");
            print_hex(elr as usize);
            serial_print_str(" fp=");
            print_hex((*frame).x[29] as usize);
            serial_print_str(" lr=");
            print_hex((*frame).x[30] as usize);
            serial_print_str("\n");
            let mut fp = (*frame).x[29] as usize;
            let mut i: u32 = 0;
            while i < 64 {
                // Bound every dereference to the mapped main-stack window.
                if fp < STACK_BASE || fp > STACK_TOP - 16 || (fp & 0x7) != 0 {
                    break;
                }
                let next = core::ptr::read_volatile(fp as *const u64) as usize;
                let ret = core::ptr::read_volatile((fp + 8) as *const u64) as usize;
                serial_print_str("[BT] ");
                print_number(i);
                serial_print_str(" ret=");
                print_hex(ret);
                serial_print_str("\n");
                // Frames ascend (stack grows down ⇒ caller fp is higher).
                if next <= fp {
                    break;
                }
                fp = next;
                i += 1;
            }
            serial_print_str("[BT] end\n");

            // Ground-truth the faulting instruction bytes from the live process
            // image (TTBR0 still active in this EL0 fault handler). Resolves the
            // base-vs-symbolization paradox: print the runtime instruction word
            // at ELR, at the return site (x30), and a small window — read only
            // within the main-image VA range so this cannot fault-in-fault.
            let dump = |addr: usize| {
                if (0x0020_0000..0x3000_0000).contains(&addr) && (addr & 0x3) == 0 {
                    let w = core::ptr::read_volatile(addr as *const u32);
                    serial_print_str("[BT] insn @");
                    print_hex(addr);
                    serial_print_str(" = ");
                    print_hex(w as usize);
                    serial_print_str("\n");
                }
            };
            let elr_u = elr as usize;
            dump(elr_u.wrapping_sub(4));
            dump(elr_u);
            dump(elr_u.wrapping_add(4));
            let lr_u = (*frame).x[30] as usize;
            dump(lr_u.wrapping_sub(8));
            dump(lr_u.wrapping_sub(4));
            dump(lr_u);
            // Module map: which VMA (base + file offset) contains the faulting PC.
            sched::dump_user_vma(elr as usize);
        }

        sched::exit(1);
    }
}

#[no_mangle]
unsafe extern "C" fn exc_unexpected_handler(esr: u64, elr: u64) {
    serial_print_str("\n[EXC] Unexpected Exception! ESR=");
    print_hex(esr as usize);
    serial_print_str(" ELR=");
    print_hex(elr as usize);
    serial_print_str("\n");
    loop { core::hint::spin_loop(); }
}
