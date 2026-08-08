//! x86-64 four-level page table (PML4 → PDPT → PD → PT, 4 KiB pages).
//!
//! Implements the IA-32e paging structures described in Intel SDM Vol 3A §4.5.

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy)]
    pub struct PageTableFlags: u64 {
        const PRESENT       = 1 << 0;
        const WRITABLE      = 1 << 1;
        const USER          = 1 << 2;
        const WRITE_THROUGH = 1 << 3;
        const NO_CACHE      = 1 << 4;
        const ACCESSED      = 1 << 5;
        const DIRTY         = 1 << 6;
        const HUGE          = 1 << 7;
        /// Bit 7 again, under the name it has in a **leaf 4 KiB PTE**: the PAT
        /// bit, the high bit of the 3-bit index (PAT:PCD:PWT) into IA32_PAT.
        ///
        /// It is the same bit as `HUGE` because the two never coexist: bit 7 is
        /// PS in a PDPTE/PDE and PAT in a PT entry, and this kernel writes leaf
        /// entries only at the PT level (`map_4k` is the only function that
        /// writes a mapping, and `ensure_table` masks intermediate entries down
        /// to PRESENT|WRITABLE|USER before storing them, so this bit can never
        /// reach a level where it would mean "huge page"). On a 2 MiB PDE the
        /// PAT bit is bit **12**, not bit 7 — if a 2 MiB mapping path is ever
        /// added it needs its own constant, not this one.
        const PAT_4K        = 1 << 7;
        const NO_EXECUTE    = 1 << 63;
    }
}

// ── IA32_PAT (MSR 0x277) ──────────────────────────────────────────────────────

/// Page Attribute Table MSR.
#[cfg(target_arch = "x86_64")]
const IA32_PAT: u32 = 0x277;

/// PAT entry encodings (SDM Vol 3A, Table 11-10).
#[cfg(target_arch = "x86_64")]
const PAT_TYPE_WC: u64 = 0x01;

/// Which IA32_PAT entry this kernel guarantees is Write Combining.
///
/// PA5 is selected by a leaf PTE with PAT=1, PCD=0, PWT=1 (index 0b101).
///
/// The choice is not arbitrary and not merely "an unused slot" — it is the slot
/// for which the write is provably harmless on *both* of our boot paths:
///
///   * Under Limine 11.4.1, PA5 is **already** WC. Both of the bootloader's
///     `wrmsr 0x277` sites (one guarded by `test edx, 0x10000`, the CPUID.01H
///     PAT feature bit) write EDX:EAX = 0x00000105:0x00070406, i.e.
///     PA0=WB PA1=WT PA2=UC- PA3=UC PA4=WP PA5=WC PA6=UC PA7=UC. Writing WC to
///     PA5 there is a value-identical store: it cannot reinterpret a single
///     existing translation, including whichever of Limine's own mappings set
///     the PAT bit to get WC (the framebuffer).
///
///   * Under our direct-boot path (`kernel/src/entry_x86_64.s`), which writes
///     only EFER and never touches IA32_PAT, the reset PAT applies and PA5 is
///     WT. There the write does change the slot — but nothing can be selecting
///     it: a PTE only reaches PA4..PA7 by setting the PAT bit, and setting that
///     bit without having programmed IA32_PAT is exactly what Limine's CPUID
///     guard exists to avoid. Our own mappings have never set it (this constant
///     is its first user), and the direct-boot page tables are 2 MiB PDEs whose
///     PAT bit (bit 12) is clear.
///
/// So on the loader path the value is unchanged, and on the direct-boot path
/// the slot has no users. Both branches leave every live translation meaning
/// exactly what it meant before.
#[cfg(target_arch = "x86_64")]
const PAT_IDX_WC: u32 = 5;

/// IA32_PAT as the BSP left it, for every AP to adopt verbatim.
///
/// The *inherited* value is not kept in a static, only printed: it is the
/// evidence for everything `PAT_IDX_WC` claims — and the one thing static
/// analysis of a binary bootloader cannot settle on its own — but nothing in
/// the kernel consumes it, and a static no code reads is a static the optimiser
/// is free to stop writing. Expect `before=0x0000010500070406
/// after=0x0000010500070406` on the Limine path (unchanged, as argued above)
/// and `before=0x0007040600070406 after=0x0007010600070406` on direct boot.
#[cfg(target_arch = "x86_64")]
pub static PAT_AFTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Set once the BSP has installed and read back a PAT with WC at `PAT_IDX_WC`.
///
/// Until it is true, `translate_flags` must not set the PAT bit in any PTE:
/// on a processor without PAT support that bit is reserved in a 4 KiB PTE and
/// setting it raises a reserved-bit #PF.
#[cfg(target_arch = "x86_64")]
static PAT_WC_READY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[cfg(target_arch = "x86_64")]
#[inline]
fn pat_wc_ready() -> bool {
    PAT_WC_READY.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
fn pat_wc_ready() -> bool { false }

/// Cacheability bits for a leaf 4 KiB PTE over a large surface the CPU only
/// ever writes in bulk — the framebuffer.
///
/// Returns Write Combining (PAT|PWT, index 0b101 = PA5) once `init_pat_bsp` has
/// confirmed PA5 really is WC, and PCD/UC- otherwise. Both are uncached, so the
/// surface stays coherent with the scanout with no cache maintenance; the
/// difference is that WC lets the CPU merge adjacent stores into burst writes,
/// and UC- forbids it. That matters because the console's cost is bulk pixel
/// movement, not individual pixels: a full-screen scroll copies the entire
/// surface, and under UC- every 4 bytes of it is a separate bus transaction.
///
/// WC is weakly ordered, so a caller that publishes the surface to a device
/// must fence first — `framebuffer::fb_flush` does.
///
/// This is deliberately not `translate_flags`: that takes `mm::PageFlags` and
/// serves the general mapping path, while `arch::init` builds the framebuffer
/// mapping from raw `PageTableFlags` before that machinery is available.
pub fn fb_cacheability_flags() -> PageTableFlags {
    if pat_wc_ready() {
        PageTableFlags::PAT_4K | PageTableFlags::WRITE_THROUGH
    } else {
        PageTableFlags::NO_CACHE
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    core::arch::asm!(
        "rdmsr",
        in("ecx")  msr,
        out("eax") lo,
        out("edx") hi,
        options(nomem, nostack, preserves_flags)
    );
    ((hi as u64) << 32) | (lo as u64)
}

/// Install `value` in IA32_PAT on the calling CPU.
///
/// SDM Vol 3A §11.12.4 defers to §11.11.8 (the MTRR change protocol) for
/// changing a PAT entry. The full protocol also enters no-fill cache mode
/// (CR0.CD=1, NW=0) around the change and synchronises every processor at a
/// rendezvous point. Both are omitted here, deliberately:
///
///   * **No-fill mode.** Its purpose is to stop *this* processor filling lines
///     under a memory type that is mid-change. That hazard requires the CPU to
///     hold cached data under a slot whose type is changing. On the BSP under
///     Limine no slot changes at all (the store is value-identical). On the
///     direct-boot path and on every AP, the slots that change have no PTE
///     selecting them at the moment of the change — see `PAT_IDX_WC` and
///     `init_pat_ap`. The `wbinvd` pair below covers the residual. CR0.CD=1 is
///     also not free under virtualisation: KVM treats it as an MMU-wide
///     memory-type event, so paying for it here would add risk, not remove it.
///
///   * **The MP rendezvous.** It exists so no two processors run with different
///     PAT values while a shared mapping is live. We get that ordering
///     structurally instead: the BSP installs its PAT at the top of
///     `arch::init`, long before `smp_init` starts any AP, and each AP installs
///     the identical value as the first thing it executes in Rust. The first
///     mapping that can select a reprogrammed slot is created by `sys_mmap` for
///     a DRM blob, which cannot happen until user space runs — by which point
///     every online CPU has the same PAT.
///
/// CR4.PGE is cleared across the CR3 reload because a `mov cr3, cr3` does not
/// invalidate global pages, and the loader may well have marked its own
/// mappings global.
#[cfg(target_arch = "x86_64")]
unsafe fn write_pat(value: u64) {
    use core::arch::asm;

    let flags = crate::arch_interrupt_save(); // reads RFLAGS, then CLI

    let cr4: u64;
    asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    let pge = cr4 & (1 << 7);
    if pge != 0 {
        asm!("mov cr4, {}", in(reg) cr4 & !(1u64 << 7), options(nostack));
    }

    asm!("wbinvd", options(nostack));
    asm!(
        "wrmsr",
        in("ecx")  IA32_PAT,
        in("eax")  value as u32,
        in("edx")  (value >> 32) as u32,
        options(nomem, nostack, preserves_flags)
    );
    asm!("wbinvd", options(nostack));
    asm!("mov {t}, cr3", "mov cr3, {t}", t = out(reg) _, options(nostack));

    if pge != 0 {
        asm!("mov cr4, {}", in(reg) cr4, options(nostack));
    }

    crate::arch_interrupt_restore(flags);
}

/// Install a PAT with Write Combining at `PAT_IDX_WC` on the BSP, and publish
/// the exact value so every AP can adopt it verbatim.
///
/// Must run before anything creates a `PageFlags::WRITECOMBINE` mapping and
/// before `smp::smp_init`; `arch::init` calls it first, ahead of even the GDT,
/// because it needs nothing but `rdmsr`/`wrmsr` and the I/O-port UART.
///
/// A processor without CPUID.01H:EDX.PAT[16] leaves `PAT_WC_READY` false and
/// `translate_flags` falls back to PCD (UC-) — exactly the behaviour that
/// shipped before this existed.
#[cfg(target_arch = "x86_64")]
pub unsafe fn init_pat_bsp() {
    use core::sync::atomic::Ordering;

    let feat = core::arch::x86_64::__cpuid(1);
    if feat.edx & (1 << 16) == 0 {
        pat_print(0, 0, false);
        return;
    }

    let before = rdmsr(IA32_PAT);
    let shift = PAT_IDX_WC * 8;
    let want = (before & !(0xFFu64 << shift)) | (PAT_TYPE_WC << shift);
    write_pat(want);

    let after = rdmsr(IA32_PAT);
    PAT_AFTER.store(after, Ordering::Relaxed);

    // Only claim WC once the CPU has actually confirmed it. A hypervisor that
    // silently discards the write leaves us on the PCD/UC- path rather than
    // handing user space a mapping we believe is WC and is not.
    let ok = (after >> shift) & 0xFF == PAT_TYPE_WC;
    PAT_WC_READY.store(ok, Ordering::Release);
    pat_print(before, after, ok);
}

/// Adopt the BSP's PAT on an Application Processor.
///
/// An AP leaves INIT/SIPI with the *reset* PAT, so without this it disagrees
/// with the BSP about every slot the loader changed — under Limine that is
/// PA4, PA5 and PA6, and PA5 is the slot Limine's own WC mappings select. Two
/// processors with different memory types for one physical page is the failure
/// mode that shows up as rare corruption rather than a fault, so this copies
/// the BSP's whole 64-bit value rather than just patching PA5.
///
/// Called as the first statement of `smp::sched_ap_entry`: at that point the AP
/// has executed only the trampoline, touching nothing but its stack and the
/// parameter block, both write-back through PA0 — a slot that is WB in the
/// reset PAT and in the loader's, and which this never changes.
#[cfg(target_arch = "x86_64")]
pub unsafe fn init_pat_ap() {
    use core::sync::atomic::Ordering;
    if !PAT_WC_READY.load(Ordering::Acquire) { return; }
    write_pat(PAT_AFTER.load(Ordering::Relaxed));
}

/// One line of boot evidence for the whole flag: what the loader left in
/// IA32_PAT, what we left, and whether WC is live.
#[cfg(target_arch = "x86_64")]
unsafe fn pat_print(before: u64, after: u64, ok: bool) {
    for b in b"[ARCH] IA32_PAT before=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(before);
    for b in b" after=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(after);
    for b in b" wc=" { crate::arch_serial_putc(*b); }
    crate::arch_serial_putc(if ok { b'1' } else { b'0' });
    crate::arch_serial_putc(b'\n');
}

pub const PAGE_SIZE: usize = 4096;

/// Mask for physical address bits in a page-table entry (bits 12-47).
/// Bits 48-51 are reserved on 4-level paging and must be zero.
const PHYS_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Map a single 4 KiB page.
///
/// `pml4_phys` is the PHYSICAL address of the PML4 root (as stored in CR3).
/// All intermediate table nodes are accessed via the HHDM so this function
/// is safe to call both before and after a user PML4 switch.
///
/// Returns `true` on success, `false` if an intermediate page-table node
/// could not be allocated (OOM).
pub unsafe fn map_4k(pml4_phys: usize, virt: usize, phys: usize, flags: PageTableFlags) -> bool {
    let pml4_idx = (virt >> 39) & 0x1FF;
    let pdpt_idx = (virt >> 30) & 0x1FF;
    let pd_idx   = (virt >> 21) & 0x1FF;
    let pt_idx   = (virt >> 12) & 0x1FF;

    let pml4 = mm::phys_to_virt(pml4_phys) as *mut u64;
    let pdpt = match ensure_table(pml4, pml4_idx, flags) { Some(p) => p, None => return false };
    let pd   = match ensure_table(pdpt, pdpt_idx, flags) { Some(p) => p, None => return false };
    let pt   = match ensure_table(pd,   pd_idx,   flags) { Some(p) => p, None => return false };

    pt.add(pt_idx).write((phys as u64 & PHYS_ADDR_MASK) | flags.bits());
    true
}

/// Unmap a single 4 KiB page and flush the TLB entry.
///
/// `pml4_phys` is the PHYSICAL address of the PML4 root.
/// All table nodes are accessed via the HHDM.
pub unsafe fn unmap_4k(pml4_phys: usize, virt: usize) {
    let pml4_idx = (virt >> 39) & 0x1FF;
    let pdpt_idx = (virt >> 30) & 0x1FF;
    let pd_idx   = (virt >> 21) & 0x1FF;
    let pt_idx   = (virt >> 12) & 0x1FF;

    let pml4 = mm::phys_to_virt(pml4_phys) as *mut u64;
    let pdpt_entry = pml4.add(pml4_idx).read();
    if pdpt_entry & PageTableFlags::PRESENT.bits() == 0 { return; }
    let pdpt = mm::phys_to_virt((pdpt_entry & PHYS_ADDR_MASK) as usize) as *mut u64;

    let pd_entry = pdpt.add(pdpt_idx).read();
    if pd_entry & PageTableFlags::PRESENT.bits() == 0 { return; }
    let pd = mm::phys_to_virt((pd_entry & PHYS_ADDR_MASK) as usize) as *mut u64;

    let pt_entry = pd.add(pd_idx).read();
    if pt_entry & PageTableFlags::PRESENT.bits() == 0 { return; }
    let pt = mm::phys_to_virt((pt_entry & PHYS_ADDR_MASK) as usize) as *mut u64;

    pt.add(pt_idx).write(0);

    // Flush the TLB entry for this virtual address.
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!("invlpg [{addr}]", addr = in(reg) virt, options(nostack));
}

/// Ensure an intermediate page-table node exists at `parent[idx]`, creating
/// it with a zeroed page if absent.
///
/// `parent` is a VIRTUAL (HHDM) pointer to the current level table.
/// Returns a VIRTUAL (HHDM) pointer to the next-level table, or `None` on OOM.
/// Intermediate PTE entries store PHYSICAL addresses (as the hardware expects).
///
/// If the entry already exists, its R/W and U/S flags are OR'd with the
/// requested flags — because all intermediate levels must reflect the union of
/// permissions needed by any child mapping.  For example, if PML4[0] was first
/// created for a read-only segment (R/W=0), a later writable mapping (stack)
/// that shares the same PML4[0] entry would be silently denied writes unless
/// the intermediate entry is upgraded here.
unsafe fn ensure_table(parent: *mut u64, idx: usize, flags: PageTableFlags) -> Option<*mut u64> {
    // Strip NO_EXECUTE; keep only P/W/U for intermediate walk entries.
    let intermediate_flags = flags & (PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER);

    let entry = parent.add(idx).read();
    if entry & PageTableFlags::PRESENT.bits() != 0 {
        // If this is a huge page, we cannot traverse deeper into it.
        if entry & PageTableFlags::HUGE.bits() != 0 {
            return None;
        }
        // Upgrade the existing entry with any newly required W/U bits.
        let upgraded = entry | intermediate_flags.bits();
        if upgraded != entry {
            parent.add(idx).write(upgraded);
        }
        let next_phys = (entry & PHYS_ADDR_MASK) as usize;
        return Some(mm::phys_to_virt(next_phys) as *mut u64);
    }
    let table_phys = alloc_zeroed_page()?;
    parent.add(idx).write((table_phys as u64 & PHYS_ADDR_MASK) | intermediate_flags.bits());
    Some(mm::phys_to_virt(table_phys) as *mut u64)
}

/// Allocate and zero a 4 KiB page for an intermediate page-table node.
/// Zeros it via the HHDM virtual address.
/// Returns the PHYSICAL address (for storage in parent PTE), or `None` on OOM.
unsafe fn alloc_zeroed_page() -> Option<usize> {
    let phys = mm::buddy::alloc(0)?;
    let virt = mm::phys_to_virt(phys) as *mut u8;
    virt.write_bytes(0, mm::buddy::PAGE_SIZE);
    Some(phys)
}

// ── arch_tlb_shootdown_all ────────────────────────────────────────────────────

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Serializes shootdown initiators so concurrent invalidations don't mix
/// their acknowledgement counts.
static TLB_LOCK: AtomicBool = AtomicBool::new(false);

/// Number of remote CPUs that still owe an acknowledgement for the current
/// shootdown round.  Set by the initiator, decremented by each target's
/// vector-0xFD handler after its CR3 reload.
static TLB_PENDING_ACKS: AtomicUsize = AtomicUsize::new(0);

/// Called from the TLB-shootdown IPI handler (vector 0xFD) after the local
/// flush completed on the target CPU.
pub fn tlb_shootdown_ack() {
    // saturating decrement: a late ack after an initiator timed out and reset
    // the counter must not wrap around and wedge the next round.
    let _ = TLB_PENDING_ACKS.fetch_update(
        Ordering::AcqRel, Ordering::Acquire,
        |v| v.checked_sub(1),
    );
}

/// Broadcast a TLB invalidation for all user-space entries to all CPUs.
///
/// `arch_set_page_table` only writes CR3 on the **current** CPU, so after
/// changing shared mappings (CoW downgrade, munmap, mprotect) every other
/// online CPU must flush too:
///
///   1. Reload CR3 locally.
///   2. If other CPUs are online: serialize initiators, arm the ack counter,
///      broadcast IPI vector 0xFD (shorthand all-excluding-self).
///   3. Spin until every target acknowledged.
///
/// The wait is **short and opportunistic**: this kernel runs syscalls and
/// the scheduler loop with IF=0, and shootdowns are frequently initiated
/// while holding the run-queue lock (mm operations run under
/// `with_*_address_space_mut`).  A target CPU spinning on that same lock
/// cannot take the IPI until the initiator releases it — so waiting "until
/// all CPUs ack" can only ever complete for targets that are idle in the
/// `sti; hlt` window (they ack within microseconds) and burns the full
/// timeout whenever any target is lock-spinning, freezing fork/exec storms.
/// On timeout we proceed: the IPI stays pended and the target flushes at its
/// next interrupt window (≤ one 10 ms tick), which precedes its next return
/// to user space.  The residual stale-TLB window only matters for another
/// thread of the same process concurrently touching the remapped page from
/// kernel context — accepted for now (the previous implementation never
/// notified other CPUs at all).
#[no_mangle]
pub unsafe extern "C" fn arch_tlb_shootdown_all() {
    #[cfg(target_arch = "x86_64")]
    {
        core::arch::asm!(
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            tmp = out(reg) _,
            options(nostack)
        );

        let ncpus = super::smp::active_cpu_count();
        if ncpus <= 1 { return; }

        while TLB_LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        TLB_PENDING_ACKS.store(ncpus - 1, Ordering::Release);
        super::smp::send_tlb_shootdown_broadcast();

        let mut spins: usize = 0;
        while TLB_PENDING_ACKS.load(Ordering::Acquire) != 0 {
            core::hint::spin_loop();
            spins += 1;
            if spins > 200_000 { break; } // opportunistic — see note above
        }

        TLB_PENDING_ACKS.store(0, Ordering::Release);
        TLB_LOCK.store(false, Ordering::Release);
    }
}

// ── Kernel page-table root ────────────────────────────────────────────────────

/// The boot (Limine) CR3 — the canonical kernel page table, captured once
/// before any user address space is loaded.  Never freed.
static KERNEL_CR3: AtomicUsize = AtomicUsize::new(0);

/// Record the currently loaded CR3 as the kernel root.  Called from arch
/// `init()` on the BSP before any user task can run.
pub unsafe fn capture_kernel_root() {
    KERNEL_CR3.store(arch_get_current_root(), Ordering::Release);
}

/// Switch this CPU back to the kernel page table.
///
/// The scheduler calls this after every switch-back so no CPU is ever left
/// idling (or reaping) on a *task's* CR3: when a task exits, its page tables
/// are freed and reused — a CPU still holding that CR3 triple-faults on its
/// next TLB miss (or on the CR3 reload in the TLB-shootdown IPI handler).
#[no_mangle]
pub unsafe extern "C" fn arch_load_kernel_page_table() {
    let root = KERNEL_CR3.load(Ordering::Acquire);
    if root == 0 { return; } // pre-capture (early boot): current CR3 is the kernel's
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!("mov cr3, {r}", r = in(reg) root, options(nostack));
}

// ── arch_set_page_table ───────────────────────────────────────────────────────

/// Load `root` into CR3.
///
/// If `root` is 0 we leave CR3 unchanged — the kernel identity map stays
/// active and there is no user-space mapping to switch away from.
/// Called by the scheduler immediately before every `cpu_switch_to` into a
/// user task, and with 0 on return to the scheduler idle loop.
#[no_mangle]
pub unsafe extern "C" fn arch_get_current_root() -> usize {
    let cr3: usize;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    cr3 & !0xFFF
}

/// On x86-64 there is one page table for the whole address space (kernel and
/// user split by VA range), so the kernel root is the same as the current root.
#[no_mangle]
pub unsafe extern "C" fn arch_get_kernel_root() -> usize {
    arch_get_current_root()
}

#[no_mangle]
pub unsafe extern "C" fn arch_set_page_table(root: usize) {
    if root != 0 {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            "mov cr3, {r}",
            r = in(reg) root as u64,
            options(nostack)
        );
    }
}

// ── arch_alloc_page_table_root ────────────────────────────────────────────────

/// Walk one page-table level for diagnostics.
/// Returns the physical address bits + flags of the entry, or 0 if not present.
unsafe fn pt_entry(table_phys: usize, hhdm: usize, idx: usize) -> u64 {
    let table = (table_phys + hhdm) as *const u64;
    table.add(idx).read()
}

/// Print a hex64 value to the COM1 serial port.
unsafe fn pt_print_hex64(v: u64) {
    for i in (0..16).rev() {
        let nibble = ((v >> (i * 4)) & 0xF) as u8;
        crate::arch_serial_putc(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 });
    }
}

/// Walk and print the 4-level page-table chain for `virt` using `pml4_phys`.
/// Prints each entry and whether it has the XD (NX) bit set.
pub unsafe fn debug_walk_pte(pml4_phys: usize, virt: usize) {
    let hhdm    = mm::phys_to_virt(0);
    let idx4    = (virt >> 39) & 0x1FF;
    let idx3    = (virt >> 30) & 0x1FF;
    let idx2    = (virt >> 21) & 0x1FF;
    let idx1    = (virt >> 12) & 0x1FF;

    // PML4
    let e4 = pt_entry(pml4_phys, hhdm, idx4);
    for b in b"  PML4[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx4 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e4);
    if e4 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
    if e4 & 1 == 0 { return; }

    // PDPT
    let pdpt_phys = (e4 & 0x000F_FFFF_FFFF_F000) as usize;
    let e3 = pt_entry(pdpt_phys, hhdm, idx3);
    for b in b"  PDPT[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx3 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e3);
    if e3 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
    if e3 & 1 == 0 { return; }
    if e3 & (1 << 7) != 0 { for b in b"  (1GB page)\n" { crate::arch_serial_putc(*b); } return; }

    // PD
    let pd_phys = (e3 & 0x000F_FFFF_FFFF_F000) as usize;
    let e2 = pt_entry(pd_phys, hhdm, idx2);
    for b in b"  PD[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx2 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e2);
    if e2 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
    if e2 & 1 == 0 { return; }
    if e2 & (1 << 7) != 0 { for b in b"  (2MB page)\n" { crate::arch_serial_putc(*b); } return; }

    // PT
    let pt_phys = (e2 & 0x000F_FFFF_FFFF_F000) as usize;
    let e1 = pt_entry(pt_phys, hhdm, idx1);
    for b in b"  PT[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx1 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e1);
    if e1 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
}

/// Allocate a zeroed 4 KiB page to serve as a process's PML4 root.
///
/// Returns the physical address of the page, or 0 on OOM.
/// Called by `sched::spawn_user` via an `extern "C"` declaration.
#[no_mangle]
pub unsafe extern "C" fn arch_alloc_page_table_root() -> usize {
    match mm::buddy::alloc(0) {
        Some(phys) => {
            let hhdm_offset = mm::phys_to_virt(0);
            let new_pml4 = (phys + hhdm_offset) as *mut u64;

            for i in 0..512 {
                new_pml4.add(i).write(0);
            }

            // Read CR3; mask off flag bits [11:0] to get the physical address.
            let cr3_raw: usize;
            core::arch::asm!("mov {}, cr3", out(reg) cr3_raw, options(nomem, nostack));
            let cr3_phys = cr3_raw & !0xFFF;

            let src_pml4 = (cr3_phys + hhdm_offset) as *const u64;
            for i in 256..512 {
                new_pml4.add(i).write(src_pml4.add(i).read());
            }

            phys
        }
        None => 0,
    }
}

// ── arch_map_page / arch_unmap_page ──────────────────────────────────────────
// Resolved at link time by mm::paging — no circular crate dependency.

/// Translate mm::PageFlags bits to x86-64 page-table flags.
fn translate_flags(bits: u64) -> PageTableFlags {
    use mm::paging::PageFlags;
    let src = PageFlags::from_bits_truncate(bits);
    let mut f = PageTableFlags::empty();
    if src.contains(PageFlags::PRESENT)  { f |= PageTableFlags::PRESENT; }
    if src.contains(PageFlags::WRITABLE) { f |= PageTableFlags::WRITABLE; }
    if src.contains(PageFlags::USER)     { f |= PageTableFlags::USER; }
    // Cacheability. The PTE carries a 3-bit index (PAT:PCD:PWT) into IA32_PAT;
    // this kernel uses exactly two of the eight entries.
    //
    // MMIO and NOCACHE take PCD alone (index 0b010 = PA2 = UC-), unchanged, and
    // deliberately first: a device BAR must stay strongly ordered even if a
    // caller also passes WRITECOMBINE. The precedence matches AArch64's.
    //
    // WRITECOMBINE takes PAT|PWT (index 0b101 = PA5), which `init_pat_bsp` has
    // guaranteed is Write Combining — real write combining, not the UC- that
    // stood in for it before there was any IA32_PAT setup here. That matters
    // because WC is what the *host* asked for: a host-visible virtio-gpu blob
    // is memory Mesa streams uploads through, and UC turns every store into its
    // own bus transaction where WC coalesces them into full-line bursts.
    //
    // If PAT could not be programmed (no CPUID.01H:EDX.PAT, or the write did
    // not stick) `pat_wc_ready` is false and this falls back to PCD/UC- — the
    // previous behaviour, which is strictly stronger and was verified correct.
    // The fallback must never be "leave both bits clear": that is PA0 = WB, a
    // cached alias of host memory, which is the bug this all exists to prevent.
    if src.contains(PageFlags::NOCACHE) || src.contains(PageFlags::MMIO) {
        f |= PageTableFlags::NO_CACHE;
    } else if src.contains(PageFlags::WRITECOMBINE) {
        if pat_wc_ready() {
            f |= PageTableFlags::PAT_4K | PageTableFlags::WRITE_THROUGH;
        } else {
            f |= PageTableFlags::NO_CACHE;
        }
    }
    // NO_EXECUTE if EXECUTE is NOT requested.
    if !src.contains(PageFlags::EXECUTE) { f |= PageTableFlags::NO_EXECUTE; }
    f
}

#[no_mangle]
pub unsafe extern "C" fn arch_map_page(
    page_table_root: usize, // physical address of PML4
    virt: usize,
    phys: usize,
    flags: u64,
) -> bool {
    let mut f = translate_flags(flags);
    f |= PageTableFlags::PRESENT; // Always set present bit for valid mappings
    map_4k(page_table_root, virt, phys, f)
}

#[no_mangle]
pub unsafe extern "C" fn arch_unmap_page(page_table_root: usize, virt: usize) {
    unmap_4k(page_table_root, virt);
}
