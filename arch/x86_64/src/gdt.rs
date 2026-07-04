//! Global Descriptor Table + Task State Segment setup.
//!
//! Segment layout (required by the SYSCALL/SYSRET convention):
//!   0x00  null
//!   0x08  kernel code64  DPL 0   ← STAR[47:32] = 0x08 (SYSCALL sets CS=0x08)
//!   0x10  kernel data    DPL 0   ← SYSCALL sets SS = 0x08+8 = 0x10
//!   0x18  user data      DPL 3   ← SYSRET sets SS = STAR[63:48]+8 = 0x10+8
//!   0x20  user code64    DPL 3   ← SYSRET sets CS = STAR[63:48]+16 = 0x10+16
//!   0x28  TSS low                ← 16-byte TSS descriptor (two consecutive slots)
//!   0x30  TSS high
//!
//! STAR MSR: bits[47:32] = 0x08, bits[63:48] = 0x10
//!   → SYSCALL: CS=0x08, SS=0x10
//!   → SYSRET64: SS=0x18|3, CS=0x20|3

use core::mem::size_of;

#[repr(C, packed)]
struct GdtEntry(u64);

/// Task State Segment — x86-64 requires one per CPU for the RSP0 kernel stack.
#[repr(C, packed)]
pub struct Tss {
    _reserved0: u32,
    /// Ring-0 stack pointer (loaded by the hardware on privilege switch).
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    _reserved1: u64,
    pub ist: [u64; 7],  // interrupt stack table
    _reserved2: u64,
    _reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Self {
            _reserved0: 0,
            rsp0: 0, rsp1: 0, rsp2: 0,
            _reserved1: 0,
            ist: [0u64; 7],
            _reserved2: 0,
            _reserved3: 0,
            iomap_base: size_of::<Tss>() as u16,
        }
    }
}

/// 16-byte TSS descriptor (two consecutive 8-byte GDT entries).
#[repr(C, packed)]
struct TssDescriptor {
    low:  u64,
    high: u64,
}

impl TssDescriptor {
    fn new(base: u64, limit: u32) -> Self {
        let limit_lo = limit & 0xFFFF;
        let limit_hi = (limit >> 16) & 0xF;
        let base_lo  = base & 0xFF_FFFF;
        let base_hi  = (base >> 24) & 0xFF;
        let base_top = base >> 32;
        // Type=0x9 (64-bit available TSS), P=1, DPL=0
        let low = (limit_lo as u64)
            | ((base_lo as u64) << 16)
            | (0x89u64 << 40)       // P=1, DPL=0, Type=9
            | ((limit_hi as u64) << 48)
            | ((base_hi as u64) << 56);
        let high = base_top;
        Self { low, high }
    }
}

#[repr(C, align(16))]
struct Gdt {
    null:        GdtEntry,
    kernel_code: GdtEntry,   // 0x08
    kernel_data: GdtEntry,   // 0x10
    user_data:   GdtEntry,   // 0x18  ← user SS for SYSRET
    user_code:   GdtEntry,   // 0x20  ← user CS for SYSRET
    tss:         TssDescriptor, // 0x28 + 0x30
}

#[repr(C, packed)]
struct GdtPointer { limit: u16, base: u64 }

/// Number of CPUs supported (must match `sched::MAX_CPUS`).
const MAX_CPUS: usize = 8;

/// Per-CPU Task State Segments.
///
/// The TSS holds RSP0, the kernel stack loaded by the hardware on a ring-3 →
/// ring-0 interrupt.  Each CPU runs a different task with a different kernel
/// stack, so a single shared TSS would make CPUs clobber each other's RSP0
/// and take user-mode interrupts on the wrong CPU's stack.
pub static mut TSS: [Tss; MAX_CPUS] = [const { Tss::new() }; MAX_CPUS];

/// Dedicated per-CPU stacks for the double-fault IST entry (IST1 = TSS.ist[0]).
///
/// A double fault is typically caused by a stack-overflow or a GP fault during
/// exception handling.  Without a separate IST stack, the double-fault handler
/// would execute on the same (possibly corrupted/exhausted) stack and
/// immediately triple-fault.  4 KiB is sufficient for the minimal handler.
static mut DOUBLE_FAULT_STACK: [[u8; 4096]; MAX_CPUS] = [[0u8; 4096]; MAX_CPUS];

/// Per-CPU GDTs.  The segment descriptors are identical on every CPU, but the
/// TSS descriptor embeds the address of that CPU's TSS, so each CPU needs its
/// own table.
static mut GDT: [Gdt; MAX_CPUS] = [const {
    Gdt {
        null:        GdtEntry(0),
        kernel_code: GdtEntry(0x00AF_9A00_0000_FFFF), // 64-bit code, DPL 0
        kernel_data: GdtEntry(0x00CF_9200_0000_FFFF), // data, DPL 0
        user_data:   GdtEntry(0x00CF_F200_0000_FFFF), // data, DPL 3
        user_code:   GdtEntry(0x00AF_FA00_0000_FFFF), // 64-bit code, DPL 3
        tss:         TssDescriptor { low: 0, high: 0 }, // filled in init_cpu()
    }
}; MAX_CPUS];

/// Initialise and load this CPU's GDT and TSS.
///
/// Called with `cpu = 0` for the BSP from `init()`; each AP calls it with its
/// own CPU index from `smp::sched_ap_entry` (after `apic::init` so the index
/// is derived from the correct LAPIC ID).
///
/// # Safety
/// `cpu` must be this CPU's unique index; must be called once per CPU.
pub unsafe fn init_cpu(cpu: usize) {
    let cpu = cpu.min(MAX_CPUS - 1);

    // IST1 (TSS.ist[0]) — dedicated double-fault stack.
    // The stack grows downward; the top is the end of the array.
    TSS[cpu].ist[0] = (core::ptr::addr_of!(DOUBLE_FAULT_STACK[cpu]) as usize
        + core::mem::size_of::<[u8; 4096]>()) as u64;

    // Patch the TSS descriptor with the runtime address of this CPU's TSS.
    let tss_base  = core::ptr::addr_of!(TSS[cpu]) as u64;
    let tss_limit = (size_of::<Tss>() - 1) as u32;
    GDT[cpu].tss = TssDescriptor::new(tss_base, tss_limit);

    #[cfg(target_arch = "x86_64")]
    let ptr = GdtPointer {
        limit: (size_of::<Gdt>() - 1) as u16,
        base:  core::ptr::addr_of!(GDT[cpu]) as u64,
    };
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!(
        "lgdt [{ptr}]",
        // Reload CS via a far return.
        "push 0x08",
        "lea {tmp}, [rip + 2f]",
        "push {tmp}",
        "retfq",
        "2:",
        // Reload data segments.
        "mov ax, 0x10",
        "mov ds, ax",
        "mov es, ax",
        "mov ss, ax",
        "xor ax, ax",
        "mov fs, ax",
        "mov gs, ax",
        // Load TSS.
        "mov ax, 0x28",
        "ltr ax",
        ptr = in(reg) &ptr,
        tmp = lateout(reg) _,
        options(nostack),
    );
}

pub fn init() {
    unsafe { init_cpu(0); }
}

/// Update RSP0 in this CPU's TSS — called by the scheduler before switching
/// to a user task so that ring-0 exceptions use the correct kernel stack.
pub fn set_kernel_stack(rsp: u64) {
    unsafe {
        let cpu = crate::smp::arch_cpu_id().min(MAX_CPUS - 1);
        TSS[cpu].rsp0 = rsp;
        // Also update the SYSCALL entry stack pointer in the per-CPU metadata.
        crate::syscall::set_syscall_stack(rsp);
    }
}

/// C-callable wrapper used by `sched` (which cannot depend on this crate
/// directly).  Resolved at link time via the linker.
#[no_mangle]
pub unsafe extern "C" fn arch_set_kernel_stack(rsp: u64) {
    set_kernel_stack(rsp);
}
