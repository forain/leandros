//! AArch64 SMP support — AP bringup via PSCI CPU_ON.
//!
//! On QEMU `-machine virt` (and most ARMv8-A platforms), secondary CPUs are
//! started with the Power State Coordination Interface (PSCI).
//!
//! The BSP calls `smp_init(mpidrs)` with a slice of target MPIDR values.
//! For each entry it:
//!   1. Allocates an 8 KiB kernel stack.
//!   2. Stores the stack top in the shared AP stack table.
//!   3. Issues a PSCI CPU_ON HVC with the AP entry address and an index
//!      that the AP uses to find its stack.
//!
//! The AP entry stub (in `global_asm!`) restores the GIC CPU interface, sets
//! up the exception vectors, and calls `sched::ap_entry()`.

// ── PSCI function IDs ─────────────────────────────────────────────────────────
/// PSCI CPU_ON (SMC64 calling convention).
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Maximum APs supported (must match sched::MAX_CPUS - 1).
pub const MAX_APS: usize = 7;

// ── Per-AP kernel stack pointer table ────────────────────────────────────────
//
// `aarch64_ap_entry` (the assembly stub below) indexes this array with the
// context ID passed by PSCI to find its stack.

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
// ── AP entry stub ─────────────────────────────────────────────────────────────
//
// PSCI delivers context_id (the AP's sequential index, 0-based) in x0.
//
// Stack layout note: AP stacks live in ap_stack_table[ap_idx].  The table
// is populated by smp_init() before CPU_ON is called for each AP.
//
// After this stub runs, each AP is in EL1 with:
//   – a valid kernel stack
//   – VBAR_EL1 pointing at __exception_vectors
//   – the GIC CPU interface enabled
//   – interrupts unmasked

.section .text
.global aarch64_ap_entry
.type   aarch64_ap_entry, %function
aarch64_ap_entry:
    // x0 = AP sequential index (0 for first AP, 1 for second, …)

    // Load this AP's kernel stack from the table.
    adrp  x1,  ap_stack_table
    add   x1,  x1, :lo12:ap_stack_table
    ldr   x1,  [x1, x0, lsl #3]   // x1 = ap_stack_table[ap_idx]
    mov   sp,  x1

    // Re-point VBAR_EL1 at our exception vectors.
    adr   x1,  __exception_vectors
    msr   VBAR_EL1, x1
    isb

    // Enable GIC CPU interface on this AP.
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
    /// Per-AP kernel stack tops, populated before each CPU_ON call.
    #[allow(improper_ctypes)]
    static mut ap_stack_table: [u64; MAX_APS];

    /// Assembly AP entry stub (defined in the global_asm! block above).
    fn aarch64_ap_entry();
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

// ── AP-side Rust wrappers ─────────────────────────────────────────────────────

/// Called from the AP entry assembly stub to enter the scheduler.
#[no_mangle]
pub extern "C" fn aarch64_sched_ap_entry() -> ! {
    sched::ap_entry()
}

/// Initialise the GIC CPU interface on a secondary CPU.
///
/// Each AP must enable its own CPU interface; the distributor was already
/// enabled by the BSP.  We reuse the existing `gic::init_cpu()` path.
#[no_mangle]
pub extern "C" fn gic_cpu_interface_init_ap() {
    // The GIC CPU interface registers are banked per-CPU.
    // gic::init() programs both the distributor and the CPU interface;
    // here we only need the CPU interface portion.
    super::gic::init_cpu_interface();
}

// ── PSCI CPU_ON ───────────────────────────────────────────────────────────────

/// Issue a PSCI CPU_ON HVC call to start the CPU identified by `mpidr`.
///
/// `entry`      — physical address of the AP entry function.
/// `context_id` — value passed to the AP in x0 on entry (our AP index).
///
/// Returns the PSCI status code (0 = success).
///
/// # Safety
/// Must be called from EL1 on a platform that implements PSCI via HVC.
#[cfg(target_arch = "aarch64")]
pub unsafe fn cpu_on(mpidr: u64, entry: usize, context_id: u64) -> i64 {
    let result: i64;
    core::arch::asm!(
        "hvc #0",
        inout("x0") PSCI_CPU_ON => result,
        in("x1") mpidr,
        in("x2") entry as u64,
        in("x3") context_id,
        options(nomem, nostack)
    );
    result
}

// ── smp_init — bring up all listed APs ───────────────────────────────────────

/// Start the APs whose MPIDRs are listed in `mpidrs`.
///
/// `mpidrs[0]` → AP index 0, `mpidrs[1]` → AP index 1, etc.
/// At most `MAX_APS` entries are processed.
///
/// # Safety
/// Must be called after the buddy allocator and GIC distributor are ready.
#[cfg(target_arch = "aarch64")]
pub unsafe fn smp_init(mpidrs: &[u64]) {
    for (i, &mpidr) in mpidrs.iter().enumerate() {
        if i >= MAX_APS { break; }

        // Allocate and zero a 64 KiB kernel stack.
        // On OOM: `continue` skips the cpu_on call for this AP, so the AP is
        // simply never started and ap_stack_table[i] remains 0.  That is safe
        // because the AP entry stub only runs after cpu_on succeeds.
        let stack = match mm::buddy::alloc(4) {
            Some(p) => p,
            None    => continue,
        };
        (stack as *mut u8).write_bytes(0, mm::buddy::PAGE_SIZE * 16);
        let stack_top = stack + mm::buddy::PAGE_SIZE * 16;

        // Store stack top before issuing CPU_ON.
        ap_stack_table[i] = stack_top as u64;

        // Issue PSCI CPU_ON: entry = aarch64_ap_entry, context = AP index.
        // PSCI success = 0; negative values are error codes.
        // PSCI_ALREADY_ON (−4) is non-fatal: the AP is already running.
        let rc = cpu_on(mpidr, aarch64_ap_entry as *const () as usize, i as u64);
        if rc != 0 && rc != -4 {
            // Roll back the stack allocation and skip this AP.
            mm::buddy::free(stack, 1);
            ap_stack_table[i] = 0;
            // PSCI error codes: -1=INTERNAL_FAILURE, -2=NOT_PRESENT,
            // -3=DENIED, -5=INVALID_ADDRESS, -6=INVALID_PARAMS.
            // We cannot use serial_print here (arch crate has no serial dep),
            // so silently continue — BSP will notice missing APs at scheduler
            // startup when the CPU count is lower than expected.
        }
    }
}
