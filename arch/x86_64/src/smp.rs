//! x86-64 SMP support — AP bringup via INIT/SIPI and a 16→64-bit trampoline.
//!
//! The BSP calls `smp_init(ncpus)` after its own subsystems are ready.
//! The function:
//!   1. Writes a minimal 32/64-bit GDT + GDTR at physical 0x7100/0x7120.
//!   2. Writes AP boot parameters (CR3, entry, per-AP stacks) at 0x7F00.
//!   3. Copies the trampoline code to physical 0x7000.
//!   4. Sends INIT+SIPI×2 to all APs via the LAPIC broadcast shorthand.
//!
//! Trampoline layout (physical page 0x7000, offsets from base):
//!   +0x000: 16-bit real-mode entry (executes at CS=0x0700, IP=0)
//!   +0x040: 32-bit protected-mode code
//!   +0x080: 64-bit long-mode code
//!
//! AP boot parameter block at physical 0x7F00:
//!   +0x00 (u64): CR3  (BSP page table root)
//!   +0x08 (u64): entry point (sched_ap_entry)
//!   +0x10 (u32): AP sequential counter (atomic xadd)
//!   +0x14 (u32): padding
//!   +0x18 (u64 × 8): kernel stack tops for APs 0..7

use super::apic;
use mm::buddy;

// ── Trampoline page address ───────────────────────────────────────────────────
const TRAMPOLINE_BASE: usize = 0x7000;

// ── AP GDT / GDTR location within the page ───────────────────────────────────
const AP_GDT_BASE:  usize = 0x7100; // 4 × 8-byte GDT entries
const AP_GDTR_ADDR: usize = 0x7120; // 2-byte limit + 4-byte base

// ── AP boot parameter block ───────────────────────────────────────────────────
const AP_CR3_OFF:    usize = 0x7F00;
const AP_ENTRY_OFF:  usize = 0x7F08;
const AP_CTR_OFF:    usize = 0x7F10;
const AP_STACKS_OFF: usize = 0x7F18;

// ── AP startup trampoline (copied to 0x7000 at runtime) ──────────────────────
//
// The global_asm! blob is placed in section .ap_trampoline; smp_init() copies
// it to physical 0x7000 before sending SIPI.
//
// Absolute physical addresses used by the trampoline:
//   0x7120 — GDTR (loaded by lgdtl in 16-bit mode)
//   0x7040 — 32-bit code entry (target of first ljmpl)
//   0x7080 — 64-bit code entry (target of second ljmpl)
//   0x7F00 — CR3
//   0x7F08 — entry function pointer
//   0x7F10 — atomic AP counter (u32)
//   0x7F18 — AP stack array (u64 × 8)
//
// GDT (written by smp_init to 0x7100):
//   0x00 — null descriptor
//   0x08 — 32-bit code  (P=1, S=1, D/B=1, G=1, Type=0xA)
//   0x10 — data         (P=1, S=1, D/B=1, G=1, Type=0x2)
//   0x18 — 64-bit code  (P=1, S=1, L=1,   G=1, Type=0xA)
core::arch::global_asm!(r#"
.section .ap_trampoline, "ax", @progbits
.code16
.global ap_trampoline_start
ap_trampoline_start:
    cli
    cld
    xor   ax, ax
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    lgdt  [0x7120]
    mov   eax, cr0
    or    al, 1
    mov   cr0, eax
    ljmp  0x08, 0x7040

.balign 0x40, 0x90
.code32
    mov   ax, 0x10
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    xor   ax, ax
    mov   fs, ax
    mov   gs, ax
    mov   eax, cr4
    or    eax, (1 << 5)
    mov   cr4, eax
    mov   eax, [0x7F00]
    mov   cr3, eax
    mov   ecx, 0xC0000080
    rdmsr
    or    eax, (1 << 8)
    wrmsr
    mov   eax, cr0
    or    eax, 0x80000000
    mov   cr0, eax
    ljmp  0x18, 0x7080

.balign 0x80, 0x90
.code64
    xor   eax, eax
    mov   ds, eax
    mov   es, eax
    mov   ss, eax
    mov   rbx, 0x7F10
    mov   eax, 1
    lock xadd [rbx], eax
    mov   rbx, 0x7F18
    mov   rsp, [rbx + rax*8]
    call  qword ptr [0x7F08]
0:  hlt
    jmp   0b

.global ap_trampoline_end
ap_trampoline_end:
"#);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end:   u8;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

unsafe fn write32(phys: usize, val: u32) {
    (phys as *mut u32).write_volatile(val);
}

unsafe fn write64(phys: usize, val: u64) {
    (phys as *mut u64).write_volatile(val);
}

// ── arch_cpu_id — provides the logical CPU index ──────────────────────────────

/// Return the LAPIC ID of the calling CPU.
///
/// For xAPIC the ID lives in bits [31:24] of the APIC ID register.
/// On QEMU/typical hardware: BSP = 0, APs = 1, 2, 3 …
///
/// Used by `sched` to index the per-CPU state arrays.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn arch_cpu_id() -> usize {
    ((apic::read(apic::LAPIC_ID) >> 24) & 0xFF) as usize
}

// ── AP entry called from the long-mode trampoline ─────────────────────────────

/// Rust-side AP entry.
///
/// Called by the 64-bit trampoline after the stack is set up.  Re-initialises
/// this AP's LAPIC (so it can receive timer/IPI interrupts) then hands off to
/// the shared scheduler loop.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn sched_ap_entry() -> ! {
    // Re-enable this AP's LAPIC (apic::init reads IA32_APIC_BASE and SVR).
    apic::init();
    // Set up per-CPU SYSCALL stack and KERNEL_GS_BASE for this AP.
    // Must come after apic::init() so arch_cpu_id() reads the correct LAPIC ID.
    super::syscall::init_ap();
    sched::ap_entry()
}

// ── smp_init — bring up ncpus application processors ─────────────────────────

/// Bring up `ncpus` Application Processors.
///
/// Must be called after the identity-mapped page table is active and the
/// physical allocator is initialised.
///
/// # Safety
/// Must be called from the BSP after `apic::init()`.  Physical addresses
/// 0x7000–0x7FFF must be identity-mapped and writeable.
#[cfg(target_arch = "x86_64")]
pub unsafe fn smp_init(ncpus: usize) {
    if ncpus == 0 { return; }
    let ncpus = ncpus.min(sched::MAX_CPUS - 1);

    // ── GDT at 0x7100 ─────────────────────────────────────────────────────────
    write64(AP_GDT_BASE + 0x00, 0x0000_0000_0000_0000); // null
    write64(AP_GDT_BASE + 0x08, 0x00CF_9A00_0000_FFFF); // 32-bit code
    write64(AP_GDT_BASE + 0x10, 0x00CF_9200_0000_FFFF); // data
    write64(AP_GDT_BASE + 0x18, 0x00AF_9A00_0000_FFFF); // 64-bit code (L=1)

    // ── GDTR at 0x7120 (limit = 0x1F, base = 0x7100) ─────────────────────────
    (AP_GDTR_ADDR as *mut u16).write_volatile(0x001F);
    ((AP_GDTR_ADDR + 2) as *mut u32).write_volatile(AP_GDT_BASE as u32);

    // ── AP boot parameters at 0x7F00 ─────────────────────────────────────────
    // CR3 — share the BSP's page table (assumed < 4 GiB for this write)
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    write64(AP_CR3_OFF, cr3);

    // Entry function pointer
    write64(AP_ENTRY_OFF, sched_ap_entry as u64);

    // AP sequential counter (starts at 0)
    write32(AP_CTR_OFF,     0);
    write32(AP_CTR_OFF + 4, 0); // padding

    // Per-AP kernel stacks (64 KiB each).
    //
    // The SIPI below is a broadcast shorthand: ALL non-BSP APs start and use
    // an atomic counter to grab a sequential index, then load
    // AP_STACKS_OFF[index] as their stack.  If any index's entry is 0 (because
    // its alloc failed) the AP starts with SP=0 and immediately faults.
    //
    // Guard: only proceed to the SIPI if EVERY requested stack was allocated.
    // If OOM prevents any allocation, skip the SIPI entirely — some APs won't
    // start, but no AP will crash with a null stack.
    let mut stacks_ok = true;
    for i in 0..ncpus {
        match buddy::alloc(4) {
            Some(stack) => {
                (stack as *mut u8).write_bytes(0, buddy::PAGE_SIZE * 16);
                write64(AP_STACKS_OFF + i * 8, (stack + buddy::PAGE_SIZE * 16) as u64);
            }
            None => { stacks_ok = false; break; }
        }
    }
    if !stacks_ok { return; }

    // ── Copy trampoline to physical 0x7000 ────────────────────────────────────
    let src = &ap_trampoline_start as *const u8;
    let end = &ap_trampoline_end   as *const u8;
    let len = end as usize - src as usize;
    core::ptr::copy_nonoverlapping(src, TRAMPOLINE_BASE as *mut u8, len);
    // Ensure trampoline is visible to APs (write-back flush via sfence).
    core::arch::asm!("sfence", options(nostack, nomem));

    // ── INIT / SIPI × 2 via LAPIC broadcast ──────────────────────────────────
    // ICR high = 0 (destination ignored when using shorthand)
    apic::write(0x310, 0);

    // INIT assert (delivery=INIT, level=assert, trigger=level, shorthand=all-excl-self)
    // 0xCC500 = (11<<18) | (1<<15) | (1<<14) | (5<<8)
    apic::write(0x300, 0x000C_C500);

    // ~10 ms spin (no calibrated delay yet)
    for _ in 0..10_000_000usize { core::hint::spin_loop(); }

    // SIPI #1 (vector 0x07 → startup at 0x7000)
    // 0xC0607 = (11<<18) | (6<<8) | 0x07
    apic::write(0x300, 0x000C_0607);

    // ~200 µs spin
    for _ in 0..200_000usize { core::hint::spin_loop(); }

    // SIPI #2 (Intel MP spec recommends two SIPIs)
    apic::write(0x300, 0x000C_0607);
}
