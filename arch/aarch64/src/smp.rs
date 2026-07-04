//! AArch64 SMP support — AP bringup via PSCI CPU_ON.
//!
//! On QEMU `-machine virt` (and most ARMv8-A platforms), secondary CPUs are
//! started with the Power State Coordination Interface (PSCI).
//!
//! The BSP calls `smp_init(mpidrs)` with a slice of target MPIDR values.
//! For each entry it:
//!   1. Allocates a 64 KiB kernel stack (stored as an HHDM virtual address).
//!   2. Records its own translation-regime registers in `AP_BOOT_PARAMS`.
//!   3. Issues a PSCI CPU_ON call with the *physical* address of the AP
//!      entry stub and an index the AP uses to find its stack.
//!
//! PSCI delivers the AP with the **MMU off**, executing the stub at its
//! physical address.  The stub therefore must not touch any Rust code (which
//! dereferences HHDM virtual pointers) until it has:
//!   – optionally dropped from EL2 to EL1 (QEMU direct `-kernel` boots can
//!     enter secondaries at EL2, mirroring `_start`),
//!   – programmed MAIR/TCR/TTBR0/TTBR1 from the BSP's values,
//!   – enabled the MMU and jumped to the *virtual* continuation.
//!
//! The instructions between "MMU on" and the virtual jump are fetched at the
//! physical PC, so smp_init identity-maps the stub's pages in a dedicated
//! TTBR0 root donated to the APs (the BSP's own TTBR0 belongs to whatever
//! user task is current and can't be relied on to identity-map anything).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// ── PSCI function IDs ─────────────────────────────────────────────────────────
/// PSCI CPU_ON (SMC64 calling convention).
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Maximum APs supported (must match sched::MAX_CPUS - 1).
pub const MAX_APS: usize = 7;

const MAX_CPUS: usize = MAX_APS + 1;

// ── Online CPU accounting ─────────────────────────────────────────────────────

/// CPUs that have completed bringup and entered the scheduler (BSP counts as 1).
static ACTIVE_CPUS: AtomicUsize = AtomicUsize::new(1);

#[no_mangle]
pub extern "C" fn arch_active_cpu_count() -> usize {
    ACTIVE_CPUS.load(Ordering::Acquire)
}

// ── CPU → MPIDR map (SMT topology) ────────────────────────────────────────────

/// MPIDR_EL1 of each logical CPU, recorded at entry.  Used by `arch_core_of`
/// to answer SMT topology queries.
static CPU_MPIDRS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Physical core of a logical CPU.
///
/// If MPIDR_EL1.MT (bit 24) is set the platform declares Aff0 to be the SMT
/// thread number and Aff1 the core — return Aff1 so the scheduler groups
/// hyperthread siblings.  QEMU `-machine virt` reports MT=0 (no SMT), which
/// degrades to the identity mapping.
#[no_mangle]
pub extern "C" fn arch_core_of(cpu: usize) -> usize {
    let mpidr = CPU_MPIDRS[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed);
    if mpidr & (1 << 24) != 0 {
        ((mpidr >> 8) & 0xFF) as usize // Aff1 = core when MT is set
    } else {
        cpu
    }
}

fn record_own_mpidr() {
    let mpidr: u64;
    unsafe {
        core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack));
    }
    let cpu = (mpidr & 0xFF) as usize;
    CPU_MPIDRS[cpu.min(MAX_CPUS - 1)].store(mpidr, Ordering::Relaxed);
}

// ── AP boot parameter block ───────────────────────────────────────────────────

/// Translation-regime state the stub copies into the AP before enabling the
/// MMU.  Filled by `smp_init` from the BSP's live registers.  Field offsets
/// are part of the assembly ABI below.
#[repr(C)]
pub struct ApBootParams {
    mair:       u64, // +0x00
    tcr:        u64, // +0x08
    ttbr0:      u64, // +0x10  (dedicated identity root for the stub pages)
    ttbr1:      u64, // +0x18
    sctlr:      u64, // +0x20  (BSP's SCTLR: MMU + caches on)
    entry_virt: u64, // +0x28  (aarch64_ap_entry_high, kernel virtual)
}

#[no_mangle]
pub static mut AP_BOOT_PARAMS: ApBootParams = ApBootParams {
    mair: 0, tcr: 0, ttbr0: 0, ttbr1: 0, sctlr: 0, entry_virt: 0,
};

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
// ── AP entry stub ─────────────────────────────────────────────────────────────
//
// PSCI delivers context_id (the AP's sequential index, 0-based) in x0, with
// the MMU off and the PC at this code's *physical* address.  Everything up to
// the `br x3` runs physically; adrp/add are PC-relative so they resolve to
// physical addresses of the statics, which is exactly what we need here.
//
// NOTE (real hardware): the BSP wrote AP_BOOT_PARAMS and the page tables with
// caches on; a physical AP would need cache maintenance before the
// non-cacheable reads below.  QEMU TCG does not model that incoherence.

.section .text
.global aarch64_ap_entry
.type   aarch64_ap_entry, %function
aarch64_ap_entry:
    // ── Drop to EL1 if PSCI entered us at EL2 (mirrors _start) ───────────
    mrs   x4, CurrentEL
    lsr   x4, x4, #2
    and   x4, x4, #3
    cmp   x4, #2
    b.ne  .Lap_at_el1

    mrs   x4, cnthctl_el2
    orr   x4, x4, #3              // EL1PCTEN | EL1PCEN
    msr   cnthctl_el2, x4
    msr   cntvoff_el2, xzr

    movz  x4, #0x8000, lsl #16    // HCR_EL2.RW: EL1 is AArch64
    msr   hcr_el2, x4

    movz  x4, #0x0800             // SCTLR_EL1 reset: RES1, MMU off
    movk  x4, #0x30d0, lsl #16
    msr   sctlr_el1, x4

    mov   x4, #0x3c5              // SPSR_EL2: EL1h, DAIF masked
    msr   spsr_el2, x4
    adr   x4, .Lap_at_el1
    msr   elr_el2, x4
    isb
    eret

.Lap_at_el1:
    msr   SPSel, #1
    isb

    // ── Program the translation regime from the BSP's snapshot ───────────
    adrp  x1, AP_BOOT_PARAMS
    add   x1, x1, :lo12:AP_BOOT_PARAMS
    ldr   x2, [x1, #0x00]
    msr   mair_el1, x2
    ldr   x2, [x1, #0x08]
    msr   tcr_el1, x2
    ldr   x2, [x1, #0x10]
    msr   ttbr0_el1, x2
    ldr   x2, [x1, #0x18]
    msr   ttbr1_el1, x2
    isb
    tlbi  vmalle1
    dsb   nsh
    isb

    // FP/SIMD on before any Rust code runs.
    mrs   x2, cpacr_el1
    orr   x2, x2, #(3 << 20)
    msr   cpacr_el1, x2

    // ── Enable the MMU and continue at the virtual alias ─────────────────
    // The fetches between the msr and the br still use the physical PC —
    // covered by the identity mapping in the donated TTBR0.
    ldr   x2, [x1, #0x20]         // SCTLR (MMU + caches on)
    ldr   x3, [x1, #0x28]         // virtual continuation
    msr   sctlr_el1, x2
    isb
    br    x3

// ── Executing at kernel-virtual addresses from here on ───────────────────────
.global aarch64_ap_entry_high
.type   aarch64_ap_entry_high, %function
aarch64_ap_entry_high:
    // x0 = AP sequential index (0 for first AP, 1 for second, …)

    // Load this AP's kernel stack (HHDM virtual) from the table.
    adrp  x1,  ap_stack_table
    add   x1,  x1, :lo12:ap_stack_table
    ldr   x1,  [x1, x0, lsl #3]   // x1 = ap_stack_table[ap_idx]
    mov   sp,  x1

    // Re-point VBAR_EL1 at our exception vectors.
    adrp  x1,  __exception_vectors
    add   x1,  x1, :lo12:__exception_vectors
    msr   VBAR_EL1, x1
    isb

    // Enable GIC CPU interface on this AP (banked registers).
    bl    gic_cpu_interface_init_ap

    // Enter the shared scheduler run loop (never returns).
    bl    aarch64_sched_ap_entry

1:  wfe
    b 1b

.global ap_stack_table
.type   ap_stack_table, %object
ap_stack_table:
    .quad 0, 0, 0, 0, 0, 0, 0     // 7 entries (one per AP, up to MAX_APS)
"#);

extern "C" {
    /// Per-AP kernel stack tops (HHDM virtual), populated before CPU_ON.
    #[allow(improper_ctypes)]
    static mut ap_stack_table: [u64; MAX_APS];

    /// Assembly AP entry stub (defined in the global_asm! block above).
    fn aarch64_ap_entry();

    /// Virtual-address continuation of the stub after the MMU is on.
    fn aarch64_ap_entry_high();
}

// ── arch_cpu_id — provides the logical CPU index ──────────────────────────────

/// Return the Aff0 field of MPIDR_EL1 — the intra-cluster CPU number.
///
/// On QEMU virt and most SoCs: core 0 → 0, core 1 → 1, …
/// Used by `sched` to index the per-CPU state arrays.
#[no_mangle]
pub unsafe extern "C" fn arch_cpu_id() -> usize {
    let mpidr: u64;
    core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack));
    (mpidr & 0xFF) as usize
}

// ── Reschedule IPI ────────────────────────────────────────────────────────────

/// Send a reschedule IPI (SGI 1) to `cpu`.  Called by `sched::trigger_preempt`.
#[no_mangle]
pub unsafe extern "C" fn arch_send_resched_ipi(cpu: usize) {
    super::gic::send_sgi(cpu, super::gic::SGI_RESCHED);
}

// ── AP-side Rust wrappers ─────────────────────────────────────────────────────

/// Called from the AP entry assembly stub to enter the scheduler.
///
/// Runs with the MMU on and a valid stack.  Starts this CPU's banked virtual
/// timer (100 Hz preemption ticks; `timer::init` also unmasks IRQs), records
/// the CPU as online, then parks in `sched::ap_entry` until the BSP opens
/// the scheduler.
#[no_mangle]
pub extern "C" fn aarch64_sched_ap_entry() -> ! {
    record_own_mpidr();
    super::timer::init();
    ACTIVE_CPUS.fetch_add(1, Ordering::AcqRel);
    sched::ap_entry()
}

/// Initialise the GIC CPU interface on a secondary CPU.
///
/// Each AP must enable its own CPU interface; the distributor was already
/// enabled by the BSP.  The banked per-CPU enables (timer PPI 27, reschedule
/// SGI 1) are set here too.
#[no_mangle]
pub extern "C" fn gic_cpu_interface_init_ap() {
    // The GIC CPU interface registers are banked per-CPU.
    // gic::init() programs both the distributor and the CPU interface;
    // here we only need the CPU interface portion.
    super::gic::init_cpu_interface();
}

// ── kernel-virtual → physical translation via AT ──────────────────────────────

/// Resolve a kernel virtual address to its physical address using the
/// hardware translation (`AT S1E1R`).  Works for kernel-image addresses in
/// both boot modes, unlike `mm::virt_to_phys` which only handles the HHDM.
unsafe fn kvirt_to_phys(va: usize) -> Option<usize> {
    let par: u64;
    core::arch::asm!(
        "at s1e1r, {va}",
        "isb",
        "mrs {par}, par_el1",
        va = in(reg) va,
        par = out(reg) par,
        options(nostack)
    );
    if par & 1 != 0 {
        None // translation failed
    } else {
        Some(((par & 0x0000_FFFF_FFFF_F000) | (va as u64 & 0xFFF)) as usize)
    }
}

// ── PSCI CPU_ON ───────────────────────────────────────────────────────────────

/// Issue a PSCI CPU_ON call to start the CPU identified by `mpidr`.
///
/// `entry`      — physical address of the AP entry function.
/// `context_id` — value passed to the AP in x0 on entry (our AP index).
///
/// Returns the PSCI status code (0 = success).
///
/// The conduit (HVC vs SMC) depends on the boot EL: when the kernel entered
/// at EL2 (QEMU direct `-kernel` with an EL2-capable CPU), QEMU registers
/// PSCI on the SMC conduit; entering at EL1 (Limine/UEFI) uses HVC.  The
/// entry stub records which one applies in `boot_entered_el2`.
///
/// # Safety
/// Must be called from EL1 on a platform that implements PSCI.
#[cfg(target_arch = "aarch64")]
pub unsafe fn cpu_on(mpidr: u64, entry: usize, context_id: u64) -> i64 {
    extern "C" {
        static boot_entered_el2: u64;
    }
    let result: i64;
    if core::ptr::read_volatile(core::ptr::addr_of!(boot_entered_el2)) != 0 {
        core::arch::asm!(
            ".inst 0xd4000003", // smc #0 (raw encoding: LLVM gates the mnemonic behind +el3)
            inout("x0") PSCI_CPU_ON => result,
            in("x1") mpidr,
            in("x2") entry as u64,
            in("x3") context_id,
            options(nomem, nostack)
        );
    } else {
        core::arch::asm!(
            "hvc #0",
            inout("x0") PSCI_CPU_ON => result,
            in("x1") mpidr,
            in("x2") entry as u64,
            in("x3") context_id,
            options(nomem, nostack)
        );
    }
    result
}

// ── smp_init — bring up all listed APs ───────────────────────────────────────

/// Start the APs whose MPIDRs are listed in `mpidrs`.
///
/// `mpidrs[0]` → AP index 0, `mpidrs[1]` → AP index 1, etc.
/// At most `MAX_APS` entries are processed.  MPIDRs that don't exist on this
/// machine simply fail CPU_ON and are skipped.
///
/// # Safety
/// Must be called after the buddy allocator and GIC distributor are ready.
#[cfg(target_arch = "aarch64")]
pub unsafe fn smp_init(mpidrs: &[u64]) {
    use super::paging::PageDescFlags;

    record_own_mpidr();

    // ── Snapshot the BSP's translation regime for the APs ────────────────────
    let (mair, tcr, ttbr1, sctlr): (u64, u64, u64, u64);
    core::arch::asm!("mrs {}, mair_el1",  out(reg) mair,  options(nomem, nostack));
    core::arch::asm!("mrs {}, tcr_el1",   out(reg) tcr,   options(nomem, nostack));
    core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1, options(nomem, nostack));
    core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));

    // ── Dedicated TTBR0 root: identity-map the stub's pages ──────────────────
    // The stub keeps fetching at its physical PC for a few instructions after
    // enabling the MMU; the BSP's own TTBR0 (a user task's table, or whatever
    // the bootloader left) can't be trusted to identity-map that.
    let entry_phys = match kvirt_to_phys(aarch64_ap_entry as *const () as usize) {
        Some(p) => p,
        None    => return, // can't resolve our own text? give up on SMP
    };
    let ap_ttbr0 = match mm::buddy::alloc(0) {
        Some(root) => {
            (mm::phys_to_virt(root) as *mut u8).write_bytes(0, mm::buddy::PAGE_SIZE);
            let flags = PageDescFlags::VALID | PageDescFlags::AF | PageDescFlags::INNER_SHR;
            // Two pages: the stub may straddle a page boundary.
            let page = entry_phys & !0xFFF;
            super::paging::map_4k(root as *mut u64, page,          page,          flags);
            super::paging::map_4k(root as *mut u64, page + 0x1000, page + 0x1000, flags);
            root as u64
        }
        None => return,
    };

    AP_BOOT_PARAMS = ApBootParams {
        mair,
        tcr,
        ttbr0: ap_ttbr0,
        ttbr1,
        sctlr,
        entry_virt: aarch64_ap_entry_high as *const () as u64,
    };
    // Make sure the params and page tables are visible before CPU_ON.
    core::arch::asm!("dsb ish", options(nostack));

    for (i, &mpidr) in mpidrs.iter().enumerate() {
        if i >= MAX_APS { break; }

        // Allocate and zero a 64 KiB kernel stack.
        let stack_phys = match mm::buddy::alloc(4) {
            Some(p) => p,
            None    => continue,
        };
        let stack_virt = mm::phys_to_virt(stack_phys);
        (stack_virt as *mut u8).write_bytes(0, mm::buddy::PAGE_SIZE * 16);

        // Store the *virtual* stack top: the AP loads SP only after the MMU
        // is on, where the HHDM mapping is guaranteed.
        ap_stack_table[i] = (stack_virt + mm::buddy::PAGE_SIZE * 16) as u64;
        core::arch::asm!("dsb ish", options(nostack));

        // Issue PSCI CPU_ON: entry = aarch64_ap_entry (physical), context = AP index.
        let rc = cpu_on(mpidr, entry_phys, i as u64);
        if rc != 0 && rc != -4 {
            // Roll back the stack allocation and skip this AP.
            // PSCI error codes: -1=NOT_SUPPORTED, -2=INVALID_PARAMS,
            // -3=DENIED, -4=ALREADY_ON, -5=ON_PENDING, -6=INTERNAL_FAILURE,
            // -7=NOT_PRESENT.  Nonexistent MPIDRs land here (fewer cores
            // than MAX_APS is the normal case, not an error).
            mm::buddy::free(stack_phys, 4);
            ap_stack_table[i] = 0;
        }
    }
}
