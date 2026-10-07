//! AArch64 SMP support — AP bringup via PSCI CPU_ON.
//!
//! On QEMU `-machine virt` (and most ARMv8-A platforms), secondary CPUs are
//! started with the Power State Coordination Interface (PSCI).
//!
//! The BSP calls `smp_init_enumerated`, which collects the AP MPIDRs (ACPI
//! MADT GICC entries, else the device tree's `/cpus`) and hands them to
//! `smp_init`. For each entry, one at a time, it:
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

/// Capacity of every per-CPU table (`sched::MAX_CPUS`).
pub const MAX_CPUS: usize = sched::MAX_CPUS;
/// Maximum APs started (everything but the BSP).
pub const MAX_APS: usize = MAX_CPUS - 1;

// ── Online CPU accounting ─────────────────────────────────────────────────────

/// CPUs that have completed bringup and entered the scheduler (BSP counts as 1).
/// `smp_init` starts APs strictly one after another, so the online CPUs are
/// always exactly the dense indices `0..ACTIVE_CPUS`.
static ACTIVE_CPUS: AtomicUsize = AtomicUsize::new(1);

#[no_mangle]
pub extern "C" fn arch_active_cpu_count() -> usize {
    ACTIVE_CPUS.load(Ordering::Acquire)
}

// ── Dense CPU index ↔ MPIDR ───────────────────────────────────────────────────
//
// Every per-CPU array is indexed by `arch_cpu_id()`, which must be dense
// (0 = BSP). `MPIDR_EL1` is not: QEMU virt numbers CPUs in Aff0 (0,1,2,…) but
// Cortex-A76/A78 parts such as the Pi 5 number *cores* in Aff1 with Aff0 = 0
// and MT set (0x81000000, 0x81000100, …), and multi-cluster parts use Aff1/
// Aff2 for the cluster — taking Aff0 as the index gave every Pi 5 core index 0.
// So the index is assigned by `smp_init` (BSP 0, APs in firmware order as
// they come up) and found again through `KEY_TO_INDEX`, a 1024-entry table
// keyed by Aff0[3:0] | Aff1[3:0] << 4 | Aff2[1:0] << 8: one `mrs` and one
// byte load, as cheap as the old `mrs` + mask. A machine whose CPUs collide
// in that key (none known) falls back to a linear search.

/// The affinity fields of an MPIDR (Aff3 [39:32], Aff2..Aff0 [23:0]).
const MPIDR_AFF_MASK: u64 = 0xFF_00FF_FFFF;

/// MPIDR (affinity bits) of each dense index; valid for `0..ASSIGNED`.
static CPU_MPIDRS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static ASSIGNED: AtomicUsize = AtomicUsize::new(1);
/// MT bit of the BSP's MPIDR (bit 24): Aff0 is an SMT thread, Aff1 the core.
static MPIDR_MT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

const KEY_BITS: usize = 10;
static KEY_TO_INDEX: [core::sync::atomic::AtomicU8; 1 << KEY_BITS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; 1 << KEY_BITS];
/// Two assigned CPUs share a key: `arch_cpu_id` searches `CPU_MPIDRS`.
static KEY_COLLISION: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const _: () = assert!(MAX_CPUS <= 256, "KEY_TO_INDEX stores u8 indices");

#[inline(always)]
fn mpidr_key(mpidr: u64) -> usize {
    ((mpidr & 0xF) | ((mpidr >> 4) & 0xF0) | ((mpidr >> 8) & 0x300)) as usize
}

#[inline(always)]
fn read_mpidr() -> u64 {
    let mpidr: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)); }
    mpidr
}

/// Give dense index `idx` (== the current `ASSIGNED`) to `mpidr`, before
/// the CPU is started. Returns the key's previous value for [`unassign`].
fn assign(idx: usize, mpidr: u64) -> u8 {
    let mpidr = mpidr & MPIDR_AFF_MASK;
    CPU_MPIDRS[idx].store(mpidr, Ordering::Relaxed);
    let key = mpidr_key(mpidr);
    let clash = (0..idx).any(|i| mpidr_key(CPU_MPIDRS[i].load(Ordering::Relaxed)) == key);
    if clash { KEY_COLLISION.store(true, Ordering::Relaxed); }
    let prev = KEY_TO_INDEX[key].load(Ordering::Relaxed);
    if !clash { KEY_TO_INDEX[key].store(idx as u8, Ordering::Relaxed); }
    ASSIGNED.store(idx + 1, Ordering::Release);
    prev
}

/// Undo the last [`assign`] (CPU_ON failed: the CPU never ran).
fn unassign(idx: usize, prev_key_val: u8) {
    ASSIGNED.store(idx, Ordering::Release);
    let key = mpidr_key(CPU_MPIDRS[idx].load(Ordering::Relaxed));
    KEY_TO_INDEX[key].store(prev_key_val, Ordering::Relaxed);
    CPU_MPIDRS[idx].store(0, Ordering::Relaxed);
}

/// Physical core of a logical CPU.
///
/// If MPIDR_EL1.MT (bit 24) is set the platform declares Aff0 to be the SMT
/// thread number and the higher levels the core — return those so the
/// scheduler groups hyperthread siblings (the Pi 5's A76 sets MT with one
/// thread per core, so each core is still distinct). Without MT (QEMU virt)
/// every CPU is its own core.
#[no_mangle]
pub extern "C" fn arch_core_of(cpu: usize) -> usize {
    if cpu >= ASSIGNED.load(Ordering::Acquire).min(MAX_CPUS) { return cpu | 1 << 40; }
    let mpidr = CPU_MPIDRS[cpu].load(Ordering::Relaxed);
    if MPIDR_MT.load(Ordering::Relaxed) {
        (mpidr >> 8) as usize // Aff3..Aff1 = core when MT is set
    } else {
        cpu
    }
}

/// `MPIDR_EL1` affinity of logical CPU `cpu`, or `None` if no CPU has that
/// index. `gic::send_sgi` needs the full affinity on GICv3, where an SGI is
/// addressed by (Aff3,Aff2,Aff1) plus a bit for Aff0 rather than by
/// CPU-interface number.
pub fn mpidr_of(cpu: usize) -> Option<u64> {
    if cpu >= ASSIGNED.load(Ordering::Acquire).min(MAX_CPUS) { return None; }
    Some(CPU_MPIDRS[cpu].load(Ordering::Relaxed))
}

/// Number of dense indices handed out (BSP + APs started or starting).
pub fn assigned() -> usize { ASSIGNED.load(Ordering::Acquire) }

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

    // GICv3 system-register interface: with ICC_SRE_EL2.Enable clear, EL1's
    // first ICC_SRE_EL1 access traps to EL2, where nothing handles it. Set
    // SRE|Enable here, gated on ID_AA64PFR0_EL1.GIC exactly as Linux's
    // init_el2 does, so a core without the interface never sees the
    // register. Harmless on a GIC-400 system: EL1's own SRE stays 0.
    mrs   x4, id_aa64pfr0_el1
    ubfx  x4, x4, #24, #4
    cbz   x4, 1f
    mrs   x4, S3_4_C12_C9_5       // ICC_SRE_EL2
    mov   x5, #0x9            // SRE | Enable (0b1001 is not a logical immediate)

    orr   x4, x4, x5
    msr   S3_4_C12_C9_5, x4
    isb
1:
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

"#);

/// Per-AP kernel stack tops (HHDM virtual), populated before CPU_ON and
/// indexed by the PSCI context id (dense CPU index - 1). Read by the stub.
#[no_mangle]
#[allow(non_upper_case_globals)]
static mut ap_stack_table: [u64; MAX_APS] = [0; MAX_APS];

extern "C" {
    /// Assembly AP entry stub (defined in the global_asm! block above).
    fn aarch64_ap_entry();

    /// Virtual-address continuation of the stub after the MMU is on.
    fn aarch64_ap_entry_high();
}

// ── arch_cpu_id — provides the logical CPU index ──────────────────────────────

/// Return the dense logical CPU index (0 = BSP, 0..active_cpu_count()) of
/// the calling CPU, looked up from `MPIDR_EL1` (see `KEY_TO_INDEX`).
///
/// Used by `sched` to index the per-CPU state arrays (hot path of
/// `current_pid()`): one system-register read and one byte load. Before
/// `smp_init` registers anything every key maps to 0, which is right: only
/// the BSP runs then.
#[no_mangle]
pub unsafe extern "C" fn arch_cpu_id() -> usize {
    let mpidr = read_mpidr();
    if KEY_COLLISION.load(Ordering::Relaxed) {
        let m = mpidr & MPIDR_AFF_MASK;
        let n = ASSIGNED.load(Ordering::Acquire).min(MAX_CPUS);
        return (0..n).find(|&i| CPU_MPIDRS[i].load(Ordering::Relaxed) == m).unwrap_or(0);
    }
    KEY_TO_INDEX[mpidr_key(mpidr)].load(Ordering::Relaxed) as usize
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
/// # Safety
/// Must be called from EL1 on a platform that implements PSCI.
#[cfg(target_arch = "aarch64")]
pub unsafe fn cpu_on(mpidr: u64, entry: usize, context_id: u64) -> i64 {
    psci_call(PSCI_CPU_ON, mpidr, entry as u64, context_id)
}

/// Issue a PSCI call `fid(a1, a2, a3)` on the platform's conduit; returns x0.
///
/// The conduit (HVC vs SMC) depends on the boot EL: when the kernel entered
/// at EL2 (QEMU direct `-kernel` with an EL2-capable CPU), QEMU registers
/// PSCI on the SMC conduit; entering at EL1 (Limine/UEFI) uses HVC.  The
/// entry stub records which one applies in `boot_entered_el2`.
///
/// # Safety
/// Must be called from EL1 on a platform that implements PSCI.
#[cfg(target_arch = "aarch64")]
pub unsafe fn psci_call(fid: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    extern "C" {
        static boot_entered_el2: u64;
    }
    let result: i64;

    // On real Raspberry Pi 5 hardware, PSCI is handled by TF-A in EL3, which
    // is reached via the SMC conduit. HVC will fault because there is no EL2 hypervisor.
    #[cfg(feature = "rpi5")]
    let use_smc = true;
    #[cfg(not(feature = "rpi5"))]
    let use_smc = core::ptr::read_volatile(core::ptr::addr_of!(boot_entered_el2)) != 0;

    if use_smc {
        core::arch::asm!(
            ".inst 0xd4000003", // smc #0 (raw encoding: LLVM gates the mnemonic behind +el3)
            inout("x0") fid => result,
            inout("x1") a1 => _,
            inout("x2") a2 => _,
            inout("x3") a3 => _,
            options(nomem, nostack)
        );
    } else {
        core::arch::asm!(
            "hvc #0",
            inout("x0") fid => result,
            inout("x1") a1 => _,
            inout("x2") a2 => _,
            inout("x3") a3 => _,
            options(nomem, nostack)
        );
    }
    result
}

// ── smp_init — bring up all listed APs ───────────────────────────────────────

/// Start the APs whose MPIDRs are listed in `mpidrs` (BSP excluded), one at
/// a time, each getting the next dense CPU index once CPU_ON accepts it.
/// At most `MAX_APS` are started; `overflow` more were enumerated but have
/// no slot (reported only). MPIDRs that don't exist fail CPU_ON and are
/// skipped without using up an index.
///
/// # Safety
/// Must be called after the buddy allocator and GIC distributor are ready.
#[cfg(target_arch = "aarch64")]
pub unsafe fn smp_init(mpidrs: &[u64], overflow: usize) {
    use super::paging::PageDescFlags;

    // The BSP is index 0 (its key already maps to 0).
    let bsp = read_mpidr();
    CPU_MPIDRS[0].store(bsp & MPIDR_AFF_MASK, Ordering::Relaxed);
    MPIDR_MT.store(bsp & (1 << 24) != 0, Ordering::Relaxed);
    if mpidrs.is_empty() {
        super::uart::serial_print_str("[SMP] 1 CPU online (no APs enumerated)\n");
        return;
    }

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
    // Make sure the params and page tables are visible before CPU_ON. The
    // stub reads AP_BOOT_PARAMS with the MMU (and so the caches) off, so
    // clean those lines to the point of coherency as well — QEMU does not
    // model the difference, real cores do.
    {
        let p = core::ptr::addr_of!(AP_BOOT_PARAMS) as usize;
        let mut line = p & !63;
        while line < p + core::mem::size_of::<ApBootParams>() {
            core::arch::asm!("dc cvac, {}", in(reg) line, options(nostack));
            line += 64;
        }
    }
    core::arch::asm!("dsb sy", options(nostack));

    extern "C" { fn print_number(n: u32); }
    let log = super::uart::serial_print_str;

    let mut next = 1usize;
    let mut failed = 0usize;
    for &mpidr in mpidrs {
        if next >= MAX_CPUS { break; }

        // Allocate and zero a 64 KiB kernel stack.
        let Some(stack_phys) = mm::buddy::alloc(4) else {
            log("[SMP] out of memory for AP stacks\n");
            break;
        };
        let stack_virt = mm::phys_to_virt(stack_phys);
        (stack_virt as *mut u8).write_bytes(0, mm::buddy::PAGE_SIZE * 16);

        // Store the *virtual* stack top: the AP loads SP only after the MMU
        // is on, where the HHDM mapping is guaranteed.
        let slot = next - 1;
        ap_stack_table[slot] = (stack_virt + mm::buddy::PAGE_SIZE * 16) as u64;
        // Its index must resolve before it runs any Rust.
        let prev_key = assign(next, mpidr);
        let before = ACTIVE_CPUS.load(Ordering::Acquire);
        core::arch::asm!("dsb ish", options(nostack));

        // Issue PSCI CPU_ON: entry = aarch64_ap_entry (physical), context = stack slot.
        let rc = cpu_on(mpidr, entry_phys, slot as u64);
        if rc != 0 {
            // PSCI error codes: -1=NOT_SUPPORTED, -2=INVALID_PARAMS,
            // -3=DENIED, -4=ALREADY_ON, -5=ON_PENDING, -6=INTERNAL_FAILURE,
            // -7=NOT_PRESENT. The CPU will not run our stub: give its index
            // and stack back. (ALREADY_ON means something else owns it.)
            log("[SMP] CPU_ON mpidr=0x");
            super::uart::print_hex(mpidr as usize);
            log(" failed rc=-");
            print_number(rc.unsigned_abs() as u32);
            log("\n");
            unassign(next, prev_key);
            mm::buddy::free(stack_phys, 4);
            ap_stack_table[slot] = 0;
            failed += 1;
            continue;
        }
        // Started: wait for it to count itself online before starting the
        // next one, so the online CPUs stay exactly `0..ACTIVE_CPUS`.
        let t0 = super::timer::monotonic_ns();
        let mut online = false;
        while super::timer::monotonic_ns().wrapping_sub(t0) < 5_000_000_000 {
            if ACTIVE_CPUS.load(Ordering::Acquire) > before { online = true; break; }
            core::hint::spin_loop();
        }
        next += 1;
        if !online {
            log("[SMP] mpidr=0x");
            super::uart::print_hex(mpidr as usize);
            log(" accepted CPU_ON but never came online; stopping AP bring-up\n");
            break;
        }
    }

    log("[SMP] ");
    print_number(arch_active_cpu_count() as u32);
    log(" CPUs online, cpu:mpidr");
    for i in 0..assigned() {
        log(" ");
        print_number(i as u32);
        log(":0x");
        super::uart::print_hex(CPU_MPIDRS[i].load(Ordering::Relaxed) as usize);
    }
    if failed != 0 { log("; "); print_number(failed as u32); log(" failed"); }
    let parked = overflow + mpidrs.len().saturating_sub(assigned() - 1 + failed);
    if parked != 0 {
        log("; ");
        print_number(parked as u32);
        log(" left parked (MAX_CPUS=");
        print_number(MAX_CPUS as u32);
        log(")");
    }
    if KEY_COLLISION.load(Ordering::Relaxed) { log("; MPIDR keys collide, linear cpu-id lookup"); }
    log("\n");
}

/// Enumerate the CPUs and start every AP: MPIDRs from the ACPI MADT (GICC
/// entries with Enabled set), else from the device tree's `/cpus`, else —
/// no tables at all — the old guess of Aff0 = 1..7 in the BSP's cluster.
/// The BSP is excluded by MPIDR, wherever it sits in the list.
///
/// # Safety
/// As [`smp_init`].
#[cfg(target_arch = "aarch64")]
pub unsafe fn smp_init_enumerated(rsdp_phys: u64, hhdm_offset: u64) {
    struct Found { bsp: u64, list: [u64; MAX_APS], n: usize, overflow: usize }
    impl Found {
        fn push(&mut self, m: u64) {
            let m = m & MPIDR_AFF_MASK;
            if m == self.bsp || self.list[..self.n].contains(&m) { return; }
            if self.n < self.list.len() { self.list[self.n] = m; self.n += 1; } else { self.overflow += 1; }
        }
    }
    let bsp = read_mpidr() & MPIDR_AFF_MASK;
    let mut f = Found { bsp, list: [0; MAX_APS], n: 0, overflow: 0 };
    let mut src = "fallback guess (no MADT/DTB)";
    if boot::acpi::for_each_madt_cpu(rsdp_phys, hhdm_offset, &mut |c| {
        if c.kind == boot::acpi::MADT_GICC && c.enabled { f.push(c.id); }
    }) && (f.n > 0 || f.overflow > 0) {
        src = "ACPI MADT";
    } else if boot::device_tree::for_each_cpu_mpidr(&mut |m| f.push(m)) && (f.n > 0 || f.overflow > 0) {
        src = "device tree";
    } else {
        f.n = 0;
        f.overflow = 0;
        for aff0 in 1..8u64 { f.push((bsp & !0xFF) | aff0); }
    }
    super::uart::serial_print_str("[CPUS] BSP mpidr=0x");
    super::uart::print_hex(bsp as usize);
    super::uart::serial_print_str(", APs from ");
    super::uart::serial_print_str(src);
    super::uart::serial_print_str("\n");
    smp_init(&f.list[..f.n], f.overflow);
}
