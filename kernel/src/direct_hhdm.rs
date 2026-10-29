//! x86_64 direct boot (`kernel-direct`: multiboot2 from kexec, multiboot1 or
//! PVH from QEMU `-kernel`): extend the HHDM past 2 GiB.
//!
//! `_start_pvh` (entry_x86_64.s) builds one PDPT at 0x21000 shared by
//! PML4[0] (identity), PML4[256] (HHDM, 0xffff_8000_0000_0000) and PML4[511]
//! (kernel image, PDPT[510..511]), with two PDs of 2 MiB pages covering
//! physical 0-2 GiB. That is all the trampoline can do without a memory map.
//! Once the map is parsed and sanitized (and before `mm` writes free-list
//! headers through the HHDM), [`extend`] gives the HHDM its own PDPT(s) and
//! maps every RAM-like range (Available, ACPI reclaimable, ACPI NVS) above
//! 2 GiB: 1 GiB pages where a whole aligned GiB is RAM and the CPU has
//! `pdpe1gb`, 2 MiB pages elsewhere. Reserved ranges and holes (MMIO: PCI
//! BARs, LAPIC, IOAPIC, ECAM, the framebuffer) are left unmapped, exactly as
//! Limine's base-revision-6 HHDM leaves them, so `paging::map_4k` and
//! `kva::ioremap` can still place 4 KiB device mappings there.
//!
//! Page-table memory comes from a static pool in the kernel's BSS: inside the
//! image, which is already reserved from the buddy and cannot overlap the
//! MBI, the initrd, the AP trampoline (0x7000) or the boot tables
//! (0x20000-0x27FFF). If the pool runs out (only conceivable with 2 MiB
//! pages and > ~60 GiB above 2 GiB) the caller drops RAM above the returned
//! limit from the memory map instead of handing the buddy unmapped memory.

use boot::{MemoryRegion, MemoryType};

const KERNEL_OFFSET: u64 = 0xffff_ffff_8000_0000;
const HHDM_L4: usize = 256;
/// First PML4 slot the HHDM must not reach (mm::kva's window).
const HHDM_L4_END: usize = 480;
const GIB: u64 = 1 << 30;
const MIB2: u64 = 2 << 20;
const LOW_DONE: u64 = 2 * GIB;
const P: u64 = 1;
const W: u64 = 1 << 1;
const PS: u64 = 1 << 7;
const ADDR: u64 = 0x000f_ffff_ffff_f000;
const POOL_PAGES: usize = 64;

#[repr(C, align(4096))]
struct Pool([[u64; 512]; POOL_PAGES]);
static mut POOL: Pool = Pool([[0; 512]; POOL_PAGES]);
static mut POOL_USED: usize = 0;

#[derive(Clone, Copy, Debug, Default)]
pub struct Outcome {
    /// End of the highest RAM-like range now mapped.
    pub top: u64,
    pub gib_pages: usize,
    pub mib2_pages: usize,
    pub tables: usize,
    /// Pool exhausted: RAM at or above this address is NOT mapped.
    pub limit: Option<u64>,
}

/// CPUID.80000001h:EDX[26] (1 GiB pages).
pub fn cpu_has_gbpages() -> bool {
    let max = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    max >= 0x8000_0001 && core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 26) != 0
}

/// A zeroed page from the pool (BSS is cleared by `_start_common`):
/// (pointer through the image mapping, physical address).
unsafe fn alloc_table() -> Option<(*mut u64, u64)> {
    if POOL_USED == POOL_PAGES { return None; }
    let t = core::ptr::addr_of_mut!(POOL.0[POOL_USED]) as *mut u64;
    POOL_USED += 1;
    Some((t, t as u64 - KERNEL_OFFSET))
}

/// Every table touched here (boot tables at 0x20000, pool pages in the
/// image at 32 MiB) lies in the identity-mapped low 2 GiB.
#[inline]
fn tab(phys: u64) -> *mut u64 { (phys & ADDR) as *mut u64 }

unsafe fn next_level(parent: *mut u64, idx: usize) -> Option<*mut u64> {
    let e = parent.add(idx).read();
    if e & P != 0 { return Some(tab(e)); }
    let (t, phys) = alloc_table()?;
    parent.add(idx).write(phys | P | W);
    Some(t)
}

fn ram_like(k: MemoryType) -> bool {
    matches!(k, MemoryType::Available | MemoryType::AcpiReclaimable | MemoryType::AcpiNvs)
}

/// Give PML4[511] a private PDPT and a private copy of the PD behind PDPT[510],
/// with the module-window entries cleared, so that `mm::kmodwin` can place
/// 4 KiB mappings in the top 2 GiB without splitting the 2 MiB entries shared
/// with the identity map and the low HHDM. Must run on the BSP before `extend`
/// (so its two pool pages are guaranteed), before SMP bring-up and before the
/// first user PML4. The kernel image (below 256 MiB, asserted by the linker
/// script) keeps its 2 MiB entries. Returns false (nothing changed) when the
/// pool cannot supply two pages.
pub unsafe fn unshare_kernel_slot() -> bool {
    let first = ((mm::kmodwin::BASE as u64 >> 21) & 511) as usize;
    let n = mm::kmodwin::SIZE >> 21;
    if POOL_PAGES - POOL_USED < 2 { return false; }
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let pml4 = tab(cr3);
    let e511 = pml4.add(511).read();
    if e511 & P == 0 { return false; }
    let old_pdpt = tab(e511);
    let e510 = old_pdpt.add(510).read();
    if e510 & P == 0 || e510 & PS != 0 { return false; }
    let old_pd = tab(e510);
    let Some((pdpt, pdpt_phys)) = alloc_table() else { return false };
    let Some((pd, pd_phys)) = alloc_table() else { return false };
    for i in 0..512 { pd.add(i).write(old_pd.add(i).read()); }
    for i in first..first + n { pd.add(i).write(0); }
    pdpt.add(510).write(pd_phys | P | W);
    pdpt.add(511).write(old_pdpt.add(511).read());
    pml4.add(511).write(pdpt_phys | P | W);
    core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    true
}

/// Map `[s, e)` (s >= 2 GiB) into the HHDM. Returns Err(addr) where the pool ran out.
unsafe fn map_run(pml4: *mut u64, s: u64, e: u64, gb: bool, o: &mut Outcome) -> Result<(), u64> {
    let mut a = s & !(MIB2 - 1);
    while a < e {
        let l4 = HHDM_L4 + (a >> 39) as usize;
        if l4 >= HHDM_L4_END { return Err(a); }
        let pdpt = next_level(pml4, l4).ok_or(a)?;
        let gi = ((a >> 30) & 511) as usize;
        let ge = pdpt.add(gi).read();
        if ge & P != 0 && ge & PS != 0 { a = (a & !(GIB - 1)) + GIB; continue; }
        if gb && ge & P == 0 && a % GIB == 0 && a + GIB <= e {
            pdpt.add(gi).write(a | P | W | PS);
            o.gib_pages += 1;
            a += GIB;
            continue;
        }
        let pd = next_level(pdpt, gi).ok_or(a)?;
        let mi = ((a >> 21) & 511) as usize;
        if pd.add(mi).read() & P == 0 {
            pd.add(mi).write(a | P | W | PS);
            o.mib2_pages += 1;
        }
        a += MIB2;
    }
    Ok(())
}

/// Extend the HHDM over `regions` (sorted, non-overlapping: the sanitized
/// map). Direct boot only, before `mm::init_with_map`.
///
/// # Safety
/// Must run on the boot page tables built by `_start_pvh`, once.
pub unsafe fn extend(regions: &[MemoryRegion], gb: bool) -> Outcome {
    let mut o = Outcome::default();
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let pml4 = tab(cr3);

    // Give the HHDM its own PDPT: the trampoline's is also the identity map
    // and the kernel image's (PDPT[510..511]), which RAM must not alias.
    let old = pml4.add(HHDM_L4).read();
    let Some((new, new_phys)) = alloc_table() else { return o };
    if old & P != 0 {
        let op = tab(old);
        for i in 0..2 { new.add(i).write(op.add(i).read()); }
    }
    pml4.add(HHDM_L4).write(new_phys | P | W);

    let mut i = 0;
    while i < regions.len() {
        if !ram_like(regions[i].kind) { i += 1; continue; }
        // Merge touching RAM-like entries into one run.
        let s = regions[i].base;
        let mut e = s + regions[i].length;
        i += 1;
        while i < regions.len() && ram_like(regions[i].kind) && regions[i].base == e {
            e += regions[i].length;
            i += 1;
        }
        let s = s.max(LOW_DONE);
        if s >= e { continue; }
        if let Err(at) = map_run(pml4, s, e, gb, &mut o) {
            o.limit = Some(at);
            break;
        }
        o.top = o.top.max(e);
    }
    o.tables = POOL_USED;
    core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    o
}
