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
const MAX_CPUS: usize = sched::MAX_CPUS;

/// Per-CPU Task State Segments.
///
/// The TSS holds RSP0, the kernel stack loaded by the hardware on a ring-3 →
/// ring-0 interrupt.  Each CPU runs a different task with a different kernel
/// stack, so a single shared TSS would make CPUs clobber each other's RSP0
/// and take user-mode interrupts on the wrong CPU's stack.
pub static mut TSS: [Tss; MAX_CPUS] = [const { Tss::new() }; MAX_CPUS];

// ── IST stacks ───────────────────────────────────────────────────────────────
//
// Three vectors run on per-CPU Interrupt Stack Table stacks instead of the
// interrupted stack: #DF (IST1), NMI (IST2) and #MC (IST3). An IST entry makes
// the CPU load RSP from the TSS unconditionally, so the handler gets a known
// good stack even when the interrupted RSP is not one -- the SYSCALL entry
// window (`syscall_entry` runs `swapgs; mov gs:[8], rsp; mov rsp, gs:[0]`
// with CS=0x08 but RSP still the *user* stack: a non-IST NMI there pushed its
// frame through the user RSP, a #PF/SMAP fault in ring 0, then #DF), or an
// overflowed kernel stack.
//
// One stack per vector, not a shared one: an IST switch always starts at the
// slot's top, so two of these vectors nesting on one stack would overwrite
// each other's frames (#MC inside the NMI handler, #DF inside either).
//
// No guard pages: the kernel has none anywhere (task stacks are HHDM buddy
// blocks, these are .bss in the kernel image, both mapped with large pages
// shared by every address space). Instead the lowest word of each stack holds
// a canary ([`IST_CANARY`]), checked by the NMI handler on every exit and by
// [`ist_canaries_ok`].

/// IST slot (1-based, as the IDT gate's IST field takes it) per vector.
pub const IST_DF: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MC: u8 = 3;

/// #DF / #MC handlers only print and halt.
const DF_STACK_SIZE: usize = 8192;
/// The NMI stack is the deepest: it also carries the kexec hand-off when an
/// AP initiates it (`kernel/src/kexec/exec_x86_64.rs::final_bsp`, entered from
/// the NMI handler through `NMI_DIVERT`), which resets PCI functions and logs
/// before jumping. `[KEXEC] ... NMI stack high-water` reports its real use.
pub const NMI_STACK_SIZE: usize = 16384;
const MC_STACK_SIZE: usize = 8192;

/// Lowest word of every IST stack.
pub const IST_CANARY: u64 = 0x4953_545F_4755_4152; // "IST_GUAR"

#[repr(C, align(4096))]
struct IstStack<const N: usize>([u8; N]);

static mut DF_STACKS: [IstStack<DF_STACK_SIZE>; MAX_CPUS] = [const { IstStack([0; DF_STACK_SIZE]) }; MAX_CPUS];
static mut NMI_STACKS: [IstStack<NMI_STACK_SIZE>; MAX_CPUS] = [const { IstStack([0; NMI_STACK_SIZE]) }; MAX_CPUS];
static mut MC_STACKS: [IstStack<MC_STACK_SIZE>; MAX_CPUS] = [const { IstStack([0; MC_STACK_SIZE]) }; MAX_CPUS];

fn df_base(cpu: usize) -> usize { unsafe { core::ptr::addr_of!(DF_STACKS[cpu]) as usize } }
fn nmi_base(cpu: usize) -> usize { unsafe { core::ptr::addr_of!(NMI_STACKS[cpu]) as usize } }
fn mc_base(cpu: usize) -> usize { unsafe { core::ptr::addr_of!(MC_STACKS[cpu]) as usize } }

/// `[lo, hi)` of `cpu`'s NMI stack (`hi` is the value in TSS.ist[IST_NMI-1]).
pub fn nmi_stack_bounds(cpu: usize) -> (u64, u64) {
    let lo = nmi_base(cpu.min(MAX_CPUS - 1)) as u64;
    (lo, lo + NMI_STACK_SIZE as u64)
}

/// All three IST canaries of `cpu` intact. Plain loads, no locks: callable
/// from the NMI handler.
pub fn ist_canaries_ok(cpu: usize) -> bool {
    let c = cpu.min(MAX_CPUS - 1);
    [df_base(c), nmi_base(c), mc_base(c)].iter()
        .all(|&b| unsafe { core::ptr::read_volatile(b as *const u64) } == IST_CANARY)
}

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

    // IST stacks (see above). The stack grows downward from the slot's end;
    // the canary sits in its lowest word. Set before this CPU loads the IDT
    // (`smp::sched_ap_entry`, `lib.rs::init`): a gate with an IST index and a
    // zero TSS slot would run the handler at RSP=0.
    for (slot, base, size) in [
        (IST_DF, df_base(cpu), DF_STACK_SIZE),
        (IST_NMI, nmi_base(cpu), NMI_STACK_SIZE),
        (IST_MC, mc_base(cpu), MC_STACK_SIZE),
    ] {
        (base as *mut u64).write_volatile(IST_CANARY);
        TSS[cpu].ist[slot as usize - 1] = (base + size) as u64;
    }

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

/// Bytes of `cpu`'s NMI stack ever written (the IST stacks are zeroed .bss;
/// the first non-zero word above the canary is the deepest point reached).
/// For the kexec log, which runs its hand-off on this stack.
pub fn nmi_stack_high_water(cpu: usize) -> usize {
    let (lo, hi) = nmi_stack_bounds(cpu);
    let mut a = lo + 8;
    while a < hi && unsafe { core::ptr::read_volatile(a as *const u64) } == 0 { a += 8; }
    (hi - a) as usize
}
