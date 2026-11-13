//! Boot information handed to the next kernel: a multiboot2 MBI (x86_64) or a
//! fresh FDT (aarch64), built with the `boot` crate builders.

use alloc::vec::Vec;

use super::stage::Alloc;
#[allow(unused_imports)]
use super::{EINVAL, ENOMEM};

/// One raw map entry: Limine's own types when Limine booted us, otherwise the
/// current (direct-boot) map translated to the same numbering.
#[derive(Clone, Copy)]
pub struct Raw { pub base: u64, pub length: u64, pub raw_type: u32 }

/// 0 usable, 1 reserved, 2 ACPI reclaimable, 3 ACPI NVS, 4 bad.
pub fn raw_map(bi: &boot::BootInfo) -> Vec<Raw> {
    let lim = boot::limine::raw_regions();
    let mut v = Vec::new();
    if !lim.is_empty() {
        for e in lim { v.push(Raw { base: e.base, length: e.length, raw_type: e.raw_type }); }
        acpi_resvd(bi, &mut v);
        return v;
    }
    use boot::MemoryType::*;
    for r in bi.memory_regions() {
        let t = match r.kind { Available => 0, Reserved => 1, AcpiReclaimable => 2, AcpiNvs => 3, BadMemory => 4 };
        v.push(Raw { base: r.base, length: r.length, raw_type: t });
    }
    v
}

/// `kexec.test=acpiresvd`: retype every ACPI reclaimable/NVS entry holding an
/// ACPI table above 2 GiB to raw 8, as firmware keeping tables in Reserved RAM
/// would. x86_64 only (aarch64 has no ACPI path).
#[allow(unused_variables)]
fn acpi_resvd(bi: &boot::BootInfo, v: &mut Vec<Raw>) {
    #[cfg(target_arch = "x86_64")]
    if super::acpi_resvd() && bi.rsdp_addr != 0 {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        boot::acpi_pages::walk(
            bi.rsdp_addr, boot::memmap::MAX_PHYS,
            &mut |p, l| ranges.push((p, l)),
            &mut |p, b| unsafe { core::ptr::copy_nonoverlapping(mm::phys_to_virt(p as usize) as *const u8, b.as_mut_ptr(), b.len()) },
        );
        let mut n = 0;
        for r in v.iter_mut() {
            if matches!(r.raw_type, 2 | 3) && ranges.iter().any(|&(p, l)| p >= (2u64 << 30) && p < r.base + r.length && p + l > r.base) {
                r.raw_type = 8;
                n += 1;
            }
        }
        crate::serial_print_str(&alloc::format!("[KEXEC] test acpiresvd: {} table range(s), {} map entr(ies) retyped to 8\n", ranges.len(), n));
    }
}

/// Sorted, merged spans of memory the old kernel may overwrite at kexec time
/// (usable, bootloader reclaimable, executable+modules).
pub fn loadable(map: &[Raw]) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = map.iter()
        .filter(|r| matches!(r.raw_type, 0 | 5 | 6) && r.length != 0)
        .map(|r| (r.base, r.base.saturating_add(r.length)))
        .collect();
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some(l) if s <= l.1 => { if e > l.1 { l.1 = e; } }
            _ => out.push((s, e)),
        }
    }
    out
}

/// x86_64 direct boot (a kexec'd kernel): the map lists this kernel's own
/// image and initrd as Reserved (the MBI builder marked them so), yet they
/// are exactly what a chained kexec overwrites. Add them to the loadable
/// spans; the purgatory runs from pages outside every destination.
#[cfg(target_arch = "x86_64")]
pub fn with_own_image(bi: &boot::BootInfo, mut spans: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    if !boot::limine::raw_regions().is_empty() { return spans; }
    const KERNEL_OFFSET: u64 = 0xffff_ffff_8000_0000;
    extern "C" { static __bss_end: u8; }
    let end = (core::ptr::addr_of!(__bss_end) as u64 - KERNEL_OFFSET + 0xfff) & !0xfff;
    spans.push((crate::direct_image_base() as u64, end));
    if bi.initrd_base != 0 && bi.initrd_size != 0 {
        spans.push((bi.initrd_base & !0xfff, (bi.initrd_base + bi.initrd_size + 0xfff) & !0xfff));
    }
    spans.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (s, e) in spans {
        match out.last_mut() {
            Some(l) if s <= l.1 => { if e > l.1 { l.1 = e; } }
            _ => out.push((s, e)),
        }
    }
    out
}

pub fn within(spans: &[(u64, u64)], s: u64, e: u64) -> bool {
    spans.iter().any(|&(a, b)| s >= a && e <= b)
}

pub struct Inputs<'a> {
    pub cmdline: &'a [u8],
    /// (dest, len)
    pub initrd: Option<(u64, u64)>,
    /// New kernel footprint ranges (start, end).
    pub footprint: &'a [(u64, u64)],
    /// The final-log buffer `(phys, size)` (`prevlog.rs`).
    pub prevlog: (u64, u64),
}

/// Build the boot info and place it. Returns its physical address.
#[cfg(target_arch = "x86_64")]
pub fn build(bi: &boot::BootInfo, map: &[Raw], inp: &Inputs, _a: &mut Alloc) -> Result<u64, isize> {
    use boot::mb2_build::*;
    let regions: Vec<RawRegion> = map.iter()
        .map(|r| RawRegion { base: r.base, length: r.length, raw_type: r.raw_type }).collect();
    let reserve: Vec<(u64, u64)> = inp.footprint.iter().map(|&(s, e)| (s, e - s)).collect();

    // RSDP bytes through the HHDM.
    let mut rsdp = [0u8; 36];
    let mut rsdp_len = 0usize;
    if bi.rsdp_addr != 0 {
        let p = mm::phys_to_virt(bi.rsdp_addr as usize) as *const u8;
        unsafe { core::ptr::copy_nonoverlapping(p, rsdp.as_mut_ptr(), 36); }
        if &rsdp[..8] == b"RSD PTR " {
            rsdp_len = if rsdp[15] >= 2 { 36 } else { 20 };
        }
    }

    // Framebuffer only when it is not RAM-backed (in a display function's
    // PCI BAR, or above all usable RAM); a RAM GOP buffer would give the new
    // kernel a black console. The kexec device reset spares that function
    // (devreset::handover_fb decides for both).
    let top = map.iter().filter(|r| r.raw_type == 0).map(|r| r.base + r.length).max().unwrap_or(0);
    let framebuffer = if super::devreset::handover_fb(bi, top).is_some() {
        Some(Mb2Fb {
            addr: bi.framebuffer_base, pitch: bi.framebuffer_pitch,
            width: bi.framebuffer_width, height: bi.framebuffer_height, bpp: 32,
        })
    } else { None };

    let input = Mb2Input {
        cmdline: inp.cmdline,
        loader_name: b"LeandrOS kexec",
        initrd: inp.initrd,
        regions: &regions,
        reserve: &reserve,
        framebuffer,
        rsdp: if rsdp_len != 0 { Some(&rsdp[..rsdp_len]) } else { None },
        efi64: bi.efi_system_table,
        prevlog: Some(inp.prevlog),
    };

    let mut scratch = unsafe {
        let mut b = alloc::boxed::Box::<Mb2Scratch>::new_uninit();
        Mb2Scratch::init_at(b.as_mut_ptr());
        b.assume_init()
    };
    let mut buf = alloc::vec![0u8; 32 * 1024];
    let len = build_mbi(&input, &mut scratch, &mut buf).map_err(|e| { crate::serial_print_str(&alloc::format!("[KEXEC] MBI build failed: {:?}\n", e)); EINVAL })?;
    drop(scratch);

    // Place below 1 MiB, in memory the old kernel does not use.
    let usable = loadable(map);
    let mut avoid: Vec<(u32, u32)> = Vec::new();
    // The MBI we were booted with holds the RSDP copy ACPI still reads.
    let (ms, me) = (crate::BOOT_MBI_RANGE[0].load(core::sync::atomic::Ordering::Relaxed),
                    crate::BOOT_MBI_RANGE[1].load(core::sync::atomic::Ordering::Relaxed));
    if me > ms && me <= 0x10_0000 { avoid.push(((ms & !0xfff) as u32, ((me + 0xfff) & !0xfff) as u32)); }
    loop {
        let p = pick_mbi_phys(len, &avoid).ok_or(ENOMEM)? as u64;
        if within(&usable, p, p + ((len as u64 + 0xfff) & !0xfff)) {
            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), mm::phys_to_virt(p as usize) as *mut u8, len); }
            return Ok(p);
        }
        let e = (p + ((len as u64 + 0xfff) & !0xfff)) as u32;
        avoid.push((p as u32, e));
        if avoid.len() > 64 { return Err(ENOMEM); }
    }
}

/// aarch64: offset of the AP park page inside the 16 KiB FDT block. The
/// secondaries' PSCI CPU_OFF is issued from a copy of the purgatory there,
/// and the mailbox the next kernel fills before each CPU_ON sits at
/// `AP_MAILBOX_OFF` in it (`purgatory_aarch64.s` explains why).
#[cfg(target_arch = "aarch64")]
pub const AP_PARK_OFF: u64 = 0x3000;
#[cfg(target_arch = "aarch64")]
pub const AP_MAILBOX_OFF: u64 = 0x800;
/// Per-CPU arrival words (8 B per dense CPU index) in the AP park page:
/// a parked CPU stores 1 once it is off old-kernel memory (purgatory_aarch64.s).
pub const AP_ARRIVED_OFF: u64 = 0x900;

/// aarch64: the FDT lives in buddy pages outside every destination range and
/// is omitted from its own `/memory`.
#[cfg(target_arch = "aarch64")]
pub fn build(bi: &boot::BootInfo, map: &[Raw], inp: &Inputs, a: &mut Alloc) -> Result<u64, isize> {
    use boot::fdt_build::*;
    use boot::memmap::{self, MemoryRegion};

    const ORDER: usize = 2; // 16 KiB: FDT in the first 12, AP park page last
    const LIMIT: usize = 8 << 30;
    // Debug knob `kexec.fdt_low=1`: put the FDT block just above the image
    // footprint (the first free 16 KiB there), which is where it ends up in
    // practice and makes the hole-accumulation bug reproducible in QEMU.
    let dtb = if crate::cmdline_knob("kexec.fdt_low") == Some("1") {
        let ird = inp.initrd.map(|(d, _)| d);
        let min = inp.footprint.iter().filter(|&&(s, _)| Some(s) != ird).map(|&(_, e)| e).max().unwrap_or(0);
        match a.block_above(ORDER, Some((min as usize + (32 << 20)).min(LIMIT)), min) {
            Ok(b) => b,
            Err(_) => a.block(ORDER, Some(LIMIT))?,
        }
    } else {
        a.block(ORDER, Some(LIMIT))?
    };
    let dtb_len = (4096usize << ORDER) as u64;
    // The whole block is left out of `/memory`, so the park page survives
    // into the next kernel (see `AP_PARK_OFF`).
    unsafe { core::ptr::write_bytes(mm::phys_to_virt(dtb + AP_PARK_OFF as usize) as *mut u8, 0, 4096); }

    // Memory map as the DT reports it: the MB2 type mapping (0/5/6 usable),
    // minus the DTB pages.
    let mut work: Vec<MemoryRegion> = map.iter()
        .map(|r| MemoryRegion { base: r.base, length: r.length, kind: boot::mb2_build::mb2_kind(r.raw_type) })
        .collect();
    work.push(MemoryRegion { base: dtb as u64, length: dtb_len, kind: boot::MemoryType::Reserved });
    // The firmware DTB's reserved ranges (BL31 on the Pi 5): this FDT has no
    // `/reserved-memory`, so they must not appear in its `/memory` at all.
    {
        let g = crate::DTB_RESERVED.lock();
        for &(s, e) in &g.0[..g.1] {
            work.push(MemoryRegion { base: s, length: e - s, kind: boot::MemoryType::Reserved });
            crate::serial_print_str(&alloc::format!("[KEXEC] carrying DTB reserved {:#x}-{:#x} (left out of /memory)\n", s, e));
        }
    }
    let n = work.len();
    let mut bounds = alloc::vec![0u64; 2 * n + 2];
    let mut out = alloc::vec![MemoryRegion::EMPTY; 2 * n + 2];
    let st = memmap::sanitize(&mut work, &mut bounds, &mut out);
    let mut mem = [(0u64, 0u64); FDT_MAX_MEMORY];
    let nmem = select_memory(&out[..st.output], &mut mem);

    let mut cpus: Vec<u64> = Vec::new();
    for i in 0..arch_aarch64::smp::assigned() {
        if let Some(m) = arch_aarch64::smp::mpidr_of(i) { cpus.push(m); }
    }
    if cpus.is_empty() { cpus.push(0); }

    extern "C" { static boot_entered_el2: u64; }
    let smc = cfg!(feature = "rpi5")
        || unsafe { core::ptr::read_volatile(core::ptr::addr_of!(boot_entered_el2)) } != 0;

    let mut seed = [0u8; 64];
    sched::random::fill(&mut seed);

    let input = FdtInput {
        bootargs: inp.cmdline,
        initrd: inp.initrd,
        rng_seed: &seed,
        memory: &mem[..nmem],
        uart_base: bi.uart_base,
        pci_ecam_base: bi.pci_ecam_base,
        ecam_size: 0x1000_0000,
        framebuffer: None,
        cpus: &cpus,
        psci_method: if smc { b"smc" } else { b"hvc" },
        kexec_ap_mailbox: dtb as u64 + AP_PARK_OFF + AP_MAILBOX_OFF,
        dma_noncoherent: !mm::dma_cache::coherent(),
        kexec_prevlog: Some(inp.prevlog),
    };
    let mut buf = alloc::vec![0u8; AP_PARK_OFF as usize];
    let len = build_fdt(&input, &mut buf).map_err(|e| { crate::serial_print_str(&alloc::format!("[KEXEC] FDT build failed: {:?}\n", e)); EINVAL })?;
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), mm::phys_to_virt(dtb) as *mut u8, len); }
    let _ = inp.footprint;
    Ok(dtb as u64)
}
