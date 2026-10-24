//! Flattened Device Tree (DTB) parser — for AArch64 QEMU and real hardware.
//!
//! QEMU -machine virt passes the DTB physical address in x0 on entry.
//! We parse just enough to extract memory regions and the UART base address.
//!
//! Spec: DeviceTree Specification v0.4 §5 (FDT format)

use super::{stash_cmdline, BootInfo, MemoryRegion, MemoryType};

/// FDT magic number.
pub const FDT_MAGIC: u32 = 0xD00DFEED;

/// FDT header (big-endian on the wire).
#[repr(C)]
struct FdtHeader {
    magic:            u32, // 0xD00DFEED
    totalsize:        u32,
    off_dt_struct:    u32,
    off_dt_strings:   u32,
    off_mem_rsvmap:   u32,
    version:          u32,
    last_comp_version: u32,
    boot_cpuid_phys:  u32,
    size_dt_strings:  u32,
    size_dt_struct:   u32,
}

/// FDT token types.
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE:   u32 = 0x00000002;
const FDT_PROP:       u32 = 0x00000003;
const FDT_NOP:        u32 = 0x00000004;
const FDT_END:        u32 = 0x00000009;

fn be32(p: *const u8) -> u32 {
    unsafe {
        u32::from_be_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
    }
}

fn be64(p: *const u8) -> u64 {
    unsafe {
        u64::from_be_bytes([
            *p, *p.add(1), *p.add(2), *p.add(3),
            *p.add(4), *p.add(5), *p.add(6), *p.add(7),
        ])
    }
}

extern "C" {
    #[allow(dead_code)]
    fn serial_write_byte(b: u8);
}

#[allow(dead_code)]
fn serial_print(msg: &str) {
    for &b in msg.as_bytes() {
        unsafe { serial_write_byte(b); }
    }
}

// ── /chosen/rng-seed ────────────────────────────────────────────────────────
//
// QEMU `virt` writes 32 bytes of host entropy to `/chosen/rng-seed` (and
// `kaslr-seed`) in the DTB it generates, and the Raspberry Pi firmware fills
// the same properties from the SoC's hardware RNG. `parse` copies them here;
// the kernel hands them to the CSPRNG once and the copy is erased.

const RNG_SEED_MAX: usize = 64;
static mut RNG_SEED: [u8; RNG_SEED_MAX] = [0; RNG_SEED_MAX];
static RNG_SEED_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

unsafe fn stash_rng_seed(data: *const u8, len: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    let at = RNG_SEED_LEN.load(Relaxed);
    let n = len.min(RNG_SEED_MAX - at);
    let dst = core::ptr::addr_of_mut!(RNG_SEED) as *mut u8;
    for i in 0..n { dst.add(at + i).write_volatile(*data.add(i)); }
    RNG_SEED_LEN.store(at + n, Relaxed);
}

/// Move the DTB-provided seed bytes (if any) into `out` and erase the kernel's
/// copy. Returns the number of bytes written; 0 when no DTB was parsed or it
/// carried no seed (x86_64, Limine/UEFI on aarch64 when the firmware passes
/// ACPI only).
pub fn take_rng_seed(out: &mut [u8; RNG_SEED_MAX]) -> usize {
    use core::sync::atomic::Ordering::Relaxed;
    let n = RNG_SEED_LEN.swap(0, Relaxed);
    let src = core::ptr::addr_of_mut!(RNG_SEED) as *mut u8;
    for i in 0..RNG_SEED_MAX {
        unsafe {
            if i < n { out[i] = src.add(i).read_volatile(); }
            src.add(i).write_volatile(0);
        }
    }
    n
}

/// Validate a DTB pointer and return true if it looks like a valid FDT.
///
/// # Safety
/// `dtb_phys` must be a readable physical address.
pub unsafe fn is_valid_dtb(dtb_virt: usize) -> bool {
    if dtb_virt == 0 || dtb_virt & 3 != 0 { return false; }
    
    // Read the magic directly from virtual memory
    let magic = be32(dtb_virt as *const u8);
    magic == FDT_MAGIC
}

/// Parse the DTB at `dtb_phys` and populate a `BootInfo`.
///
/// # Safety
/// `dtb_phys` must be the physical address of a valid, complete FDT blob.
pub unsafe fn parse(dtb_phys: usize) -> BootInfo {
    let mut info = BootInfo {
        memory_map:          core::ptr::null(),
        memory_map_len:      0,
        framebuffer_base:    0,
        framebuffer_width:   0,
        framebuffer_height:  0,
        framebuffer_pitch:   0,
        framebuffer_size:    0,
        rsdp_addr:           0,
        uart_base:           0,
        pci_ecam_base:       0,
        initrd_base:         0,
        initrd_size:         0,
        hhdm_offset:         0,
        cmdline_ptr:         0,
        cmdline_len:         0,
        efi_system_table:    0,
    };

    if dtb_phys == 0 || !is_valid_dtb(dtb_phys) {
        return info;
    }
    remember_dtb(dtb_phys);
    collect_reserved(dtb_phys);

    // 64 slots
    static mut MM: [MemoryRegion; 64] = [MemoryRegion {
        base: 0, length: 0, kind: MemoryType::Reserved
    }; 64];
    let mut mm_idx = 0usize;

    let hdr = dtb_phys as *const FdtHeader;
    let struct_off   = be32(core::ptr::addr_of!((*hdr).off_dt_struct)   as *const u8) as usize;
    let strings_off  = be32(core::ptr::addr_of!((*hdr).off_dt_strings)  as *const u8) as usize;
    
    let dt_struct    = (dtb_phys + struct_off) as *const u8;
    let dt_strings   = (dtb_phys + strings_off) as *const u8;

    let mut ptr = dt_struct;
    let mut depth = 0;
    let mut in_chosen = false;
    let mut in_memory = false;
    let mut in_pl011  = false;
    let mut in_framebuf = false;
    let mut in_pcie = false;

    let mut address_cells = 2; 
    let mut size_cells = 2;    
    
    let mut initrd_start: u64 = 0;
    let mut initrd_end: u64 = 0;

    loop {
        let token = be32(ptr);
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let name_ptr = ptr.add(4);
                let name_len = strlen(name_ptr);
                let name = core::slice::from_raw_parts(name_ptr, name_len);
                
                in_chosen   = name == b"chosen" || name.starts_with(b"chosen@");
                in_memory   = name.starts_with(b"memory@") || name == b"memory";
                in_pl011    = name.starts_with(b"pl011@") || name.starts_with(b"uart@");
                in_framebuf = name.starts_with(b"framebuffer@") || name == b"framebuffer";
                in_pcie     = name.starts_with(b"pcie@") || name == b"pcie";

                ptr = ptr.add(4 + align_up(name_len + 1, 4));
            }
            FDT_END_NODE => {
                depth -= 1;
                if in_chosen {
                    info.initrd_base = initrd_start;
                    if initrd_end > initrd_start {
                        info.initrd_size = initrd_end - initrd_start;
                    }
                }
                in_chosen = false;
                in_memory = false;
                in_pl011 = false;
                in_framebuf = false;
                if depth == 0 { break; }
                ptr = ptr.add(4);
            }
            FDT_PROP => {
                let data_len = be32(ptr.add(4)) as usize;
                let name_off = be32(ptr.add(8)) as usize;
                let name_ptr = dt_strings.add(name_off);
                let pn_len   = strlen(name_ptr);
                let data_ptr = ptr.add(12);
                let prop_name = core::slice::from_raw_parts(name_ptr, pn_len);

                match prop_name {
                    b"#address-cells" if depth == 1 => { address_cells = be32(data_ptr) as u8; }
                    b"#size-cells" if depth == 1 => { size_cells = be32(data_ptr) as u8; }
                    
                    b"reg" if in_framebuf && data_len >= 8 => {
                        info.framebuffer_base = if address_cells >= 2 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }
                    b"width" if in_framebuf && data_len >= 4 => { info.framebuffer_width = be32(data_ptr); }
                    b"height" if in_framebuf && data_len >= 4 => { info.framebuffer_height = be32(data_ptr); }
                    b"stride" if in_framebuf && data_len >= 4 => { info.framebuffer_pitch = be32(data_ptr); }

                    b"compatible" => {
                    }

                    b"reg" if in_pl011 && data_len >= 4 => {
                        let base = if address_cells >= 2 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                        info.uart_base = base;
                    }

                    b"bus-range" if in_pcie && data_len >= 8
                        && PCI_BUS_RANGE.load(core::sync::atomic::Ordering::Relaxed) == u64::MAX => {
                        let (s0, e0) = (be32(data_ptr) as u64 & 0xff, be32(data_ptr.add(4)) as u64 & 0xff);
                        PCI_BUS_RANGE.store((s0 << 32) | e0, core::sync::atomic::Ordering::Relaxed);
                    }

                    b"reg" if in_pcie && data_len >= 8 => {
                        let mut off = 0;
                        while off + 8 <= data_len {
                            let base = if address_cells >= 2 { be64(data_ptr.add(off)) } else { be32(data_ptr.add(off)) as u64 };
                            
                            // If this is the first one, assume it's ECAM
                            if info.pci_ecam_base == 0 {
                                info.pci_ecam_base = base;
                            }
                            
                            off += (address_cells + size_cells) as usize * 4;
                        }
                    }

                    b"reg" if in_memory && mm_idx < 60 => {
                        let entry_bytes = (address_cells + size_cells) as usize * 4;
                        if entry_bytes > 0 {
                            let mut off = 0;
                            while off + entry_bytes <= data_len {
                                let base = if address_cells == 2 { be64(data_ptr.add(off)) } else { be32(data_ptr.add(off)) as u64 };
                                let size_off = off + address_cells as usize * 4;
                                let size = if size_cells == 2 { be64(data_ptr.add(size_off)) } else { be32(data_ptr.add(size_off)) as u64 };
                                unsafe {
                                    MM[mm_idx] = MemoryRegion { base, length: size, kind: MemoryType::Available };
                                    mm_idx += 1;
                                }
                                off += entry_bytes;
                            }
                        }
                    }

                    b"rng-seed" | b"kaslr-seed" if in_chosen && depth == 2 => {
                        stash_rng_seed(data_ptr, data_len);
                    }
                    b"leandros,kexec-prevlog" if in_chosen && data_len >= 16 => {
                        super::set_kexec_prevlog(be64(data_ptr), be64(data_ptr.add(8)));
                    }
                    b"leandros,kexec-ap-mailbox" if in_chosen && data_len >= 8 => {
                        KEXEC_AP_MAILBOX.store(be64(data_ptr), core::sync::atomic::Ordering::Relaxed);
                    }
                    b"linux,initrd-start" if in_chosen && data_len >= 4 => {
                        initrd_start = if data_len >= 8 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }
                    b"bootargs" if in_chosen && data_len > 1 => {
                        stash_cmdline(&mut info, core::slice::from_raw_parts(data_ptr, data_len));
                    }
                    b"linux,initrd-end" if in_chosen && data_len >= 4 => {
                        initrd_end = if data_len >= 8 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }
                    _ => {}
                }
                ptr = ptr.add(12 + align_up(data_len, 4));
            }
            FDT_NOP => { ptr = ptr.add(4); }
            FDT_END => { break; }
            _ => { break; }
        }
    }

    info.memory_map = core::ptr::addr_of!(MM) as *const MemoryRegion;
    info.memory_map_len = mm_idx;
    info
}

// ── /chosen/leandros,kexec-ap-mailbox ───────────────────────────────────────

/// Physical address of the AP mailbox a kexec'ing LeandrOS left in a
/// reserved page (`kernel/src/kexec/purgatory_aarch64.s`), 0 = none.
static KEXEC_AP_MAILBOX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// See [`KEXEC_AP_MAILBOX`].
pub fn kexec_ap_mailbox() -> u64 {
    KEXEC_AP_MAILBOX.load(core::sync::atomic::Ordering::Relaxed)
}

// ── CPU enumeration ───────────────────────────────────────────────────────────

/// Kernel-virtual address of the last valid DTB `parse` saw, if it was a
/// higher-half (HHDM) address that stays mapped after boot. 0 = none.
static DTB_VIRT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn remember_dtb(addr: usize) {
    // A low (physical / identity) address may not stay mapped once the
    // kernel's own tables are live; only keep a higher-half one.
    if addr >= 0xFFFF_0000_0000_0000 {
        DTB_VIRT.store(addr, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Call `f(mpidr)` for every available `/cpus/cpu@*` node (`device_type =
/// "cpu"`, `status` absent or "okay"), in tree order; the value is the
/// node's `reg`, which on Arm is the MPIDR affinity. Returns `false` if no
/// DTB was recorded or it has no `/cpus` node.
///
/// # Safety
/// The DTB recorded by `parse` must still be mapped.
pub unsafe fn for_each_cpu_mpidr(f: &mut dyn FnMut(u64)) -> bool {
    let dtb = DTB_VIRT.load(core::sync::atomic::Ordering::Relaxed);
    for_each_cpu_mpidr_at(dtb, f)
}

/// [`for_each_cpu_mpidr`] on an explicit DTB address (host tests).
///
/// # Safety
/// `dtb` must be 0 or the address of a readable, complete FDT.
pub unsafe fn for_each_cpu_mpidr_at(dtb: usize, f: &mut dyn FnMut(u64)) -> bool {
    if dtb == 0 || !is_valid_dtb(dtb) { return false; }
    let hdr = dtb as *const FdtHeader;
    let struct_off  = be32(core::ptr::addr_of!((*hdr).off_dt_struct)  as *const u8) as usize;
    let strings_off = be32(core::ptr::addr_of!((*hdr).off_dt_strings) as *const u8) as usize;
    let total       = be32(core::ptr::addr_of!((*hdr).totalsize)      as *const u8) as usize;
    let mut ptr = (dtb + struct_off) as *const u8;
    let end = (dtb + total) as *const u8;
    let strings = (dtb + strings_off) as *const u8;

    // Depth convention as in `parse`: the root node is depth 1, /cpus is
    // depth 2 and each cpu@N node depth 3.
    let mut depth = 0usize;
    let mut in_cpus = false;
    let mut found_cpus = false;
    let mut addr_cells = 1usize;
    // Current cpu@N node: (is a cpu node, device_type == cpu, available, reg)
    let mut cur_cpu = false;
    let mut is_cpu_type = false;
    let mut available = true;
    let mut reg: Option<u64> = None;

    while ptr < end {
        match be32(ptr) {
            FDT_BEGIN_NODE => {
                depth += 1;
                let name_ptr = ptr.add(4);
                let name_len = strlen(name_ptr);
                let name = core::slice::from_raw_parts(name_ptr, name_len);
                if depth == 2 && name == b"cpus" { in_cpus = true; found_cpus = true; }
                if in_cpus && depth == 3 {
                    cur_cpu = name.starts_with(b"cpu@") || name == b"cpu";
                    is_cpu_type = false;
                    available = true;
                    reg = None;
                }
                ptr = ptr.add(4 + align_up(name_len + 1, 4));
            }
            FDT_END_NODE => {
                if in_cpus && depth == 3 && cur_cpu {
                    if is_cpu_type && available {
                        if let Some(r) = reg { f(r); }
                    }
                    cur_cpu = false;
                }
                if in_cpus && depth == 2 { in_cpus = false; }
                depth = depth.saturating_sub(1);
                if depth == 0 { break; }
                ptr = ptr.add(4);
            }
            FDT_PROP => {
                let len = be32(ptr.add(4)) as usize;
                let name_ptr = strings.add(be32(ptr.add(8)) as usize);
                let pname = core::slice::from_raw_parts(name_ptr, strlen(name_ptr));
                let data = ptr.add(12);
                if in_cpus && depth == 2 && pname == b"#address-cells" && len >= 4 {
                    addr_cells = be32(data) as usize;
                }
                if in_cpus && depth == 3 && cur_cpu {
                    match pname {
                        b"device_type" => { is_cpu_type = len >= 3 && core::slice::from_raw_parts(data, 3) == b"cpu"; }
                        b"status" => {
                            available = len >= 4 && (core::slice::from_raw_parts(data, 4) == b"okay"
                                || (len >= 2 && core::slice::from_raw_parts(data, 2) == b"ok"));
                        }
                        b"reg" if len >= 4 => {
                            reg = Some(if addr_cells >= 2 && len >= 8 { be64(data) } else { be32(data) as u64 });
                        }
                        _ => {}
                    }
                }
                ptr = ptr.add(12 + align_up(len, 4));
            }
            FDT_NOP => { ptr = ptr.add(4); }
            _ => break,
        }
    }
    found_cpus
}

fn strlen(s: *const u8) -> usize {
    let mut len = 0;
    unsafe { while *s.add(len) != 0 { len += 1; } }
    len
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

// ── /memreserve/ and /reserved-memory (lane dtbres) ─────────────────────────

/// One physical range the DTB says the OS must not allocate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbReserved {
    pub base: u64,
    pub size: u64,
    /// `/reserved-memory` child with `no-map` (never map it cacheable).
    pub no_map: bool,
    /// From the header's `/memreserve/` block (else a `/reserved-memory` child).
    pub memreserve: bool,
    /// Node name (`/reserved-memory` children), NUL-padded, truncated.
    pub name: [u8; 24],
}

impl DtbReserved {
    pub fn name_str(&self) -> &str {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }
}

const DTB_RSV_MAX: usize = 32;
static mut DTB_RSV: [Option<DtbReserved>; DTB_RSV_MAX] = [None; DTB_RSV_MAX];
static DTB_RSV_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// `bus-range` of the first `pcie` node, (start, end); end 0xFFFF_FFFF = none.
static PCI_BUS_RANGE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);

/// Reserved ranges recorded by the last `parse` (it reads them while the
/// blob is surely mapped). Early boot: callers reserve these before the
/// allocator is seeded.
pub fn reserved_ranges(out: &mut dyn FnMut(&DtbReserved)) -> usize {
    let n = DTB_RSV_LEN.load(core::sync::atomic::Ordering::Relaxed);
    for i in 0..n {
        if let Some(r) = unsafe { (*core::ptr::addr_of!(DTB_RSV))[i] } { out(&r); }
    }
    n
}

/// `bus-range = <start end>` of the DTB's PCIe host node, if `parse` saw one.
pub fn pci_bus_range() -> Option<(u8, u8)> {
    let v = PCI_BUS_RANGE.load(core::sync::atomic::Ordering::Relaxed);
    if v == u64::MAX { return None; }
    Some(((v >> 32) as u8, v as u8))
}

unsafe fn collect_reserved(dtb: usize) {
    let mut n = 0usize;
    for_each_reserved_at(dtb, &mut |r| {
        if n < DTB_RSV_MAX {
            (*core::ptr::addr_of_mut!(DTB_RSV))[n] = Some(*r);
            n += 1;
        }
    });
    DTB_RSV_LEN.store(n, core::sync::atomic::Ordering::Relaxed);
}

fn read_cells(p: *const u8, cells: usize) -> u64 {
    match cells { 0 => 0, 1 => be32(p) as u64, _ => be64(unsafe { p.add((cells - 2) * 4) }) }
}

/// Call `f` for every `/memreserve/` entry, then for every `reg` entry of
/// every enabled `/reserved-memory` child (dynamic `size`/`alloc-ranges`
/// children without `reg` are skipped: nothing to reserve until the OS picks a
/// spot). Returns `false` if `dtb` is not a valid FDT.
///
/// # Safety
/// `dtb` must be 0 or the address of a readable, complete FDT.
pub unsafe fn for_each_reserved_at(dtb: usize, f: &mut dyn FnMut(&DtbReserved)) -> bool {
    if dtb == 0 || !is_valid_dtb(dtb) { return false; }
    let hdr = dtb as *const FdtHeader;
    let rsv_off     = be32(core::ptr::addr_of!((*hdr).off_mem_rsvmap) as *const u8) as usize;
    let struct_off  = be32(core::ptr::addr_of!((*hdr).off_dt_struct)  as *const u8) as usize;
    let strings_off = be32(core::ptr::addr_of!((*hdr).off_dt_strings) as *const u8) as usize;
    let total       = be32(core::ptr::addr_of!((*hdr).totalsize)      as *const u8) as usize;

    // /memreserve/: (u64 address, u64 size) pairs up to a (0, 0) terminator.
    if rsv_off != 0 && rsv_off < total {
        let mut p = dtb + rsv_off;
        while p + 16 <= dtb + total {
            let (base, size) = (be64(p as *const u8), be64((p + 8) as *const u8));
            if base == 0 && size == 0 { break; }
            if size != 0 {
                f(&DtbReserved { base, size, no_map: false, memreserve: true, name: [0; 24] });
            }
            p += 16;
        }
    }

    let mut ptr = (dtb + struct_off) as *const u8;
    let end = (dtb + total) as *const u8;
    let strings = (dtb + strings_off) as *const u8;
    // Root = depth 1, /reserved-memory = depth 2, its children = depth 3.
    let mut depth = 0usize;
    let (mut root_ac, mut root_sc) = (2usize, 1usize);
    let mut in_rm = false;
    let (mut rm_ac, mut rm_sc) = (0usize, 0usize);
    // Current child: name, reg data, no-map, disabled.
    let mut name = [0u8; 24];
    let mut reg: (*const u8, usize) = (core::ptr::null(), 0);
    let mut no_map = false;
    let mut disabled = false;
    while ptr < end {
        match be32(ptr) {
            FDT_BEGIN_NODE => {
                depth += 1;
                let np = ptr.add(4);
                let nl = strlen(np);
                let nm = core::slice::from_raw_parts(np, nl);
                if depth == 2 {
                    in_rm = nm == b"reserved-memory" || nm.starts_with(b"reserved-memory@");
                    if in_rm { rm_ac = root_ac; rm_sc = root_sc; }
                } else if depth == 3 && in_rm {
                    name = [0; 24];
                    let k = nl.min(23);
                    name[..k].copy_from_slice(&nm[..k]);
                    reg = (core::ptr::null(), 0);
                    no_map = false;
                    disabled = false;
                }
                ptr = ptr.add(4 + align_up(nl + 1, 4));
            }
            FDT_END_NODE => {
                if depth == 3 && in_rm && !disabled && !reg.0.is_null() {
                    let step = (rm_ac + rm_sc) * 4;
                    let mut off = 0usize;
                    while step > 0 && off + step <= reg.1 {
                        let base = read_cells(reg.0.add(off), rm_ac);
                        let size = read_cells(reg.0.add(off + rm_ac * 4), rm_sc);
                        if size != 0 {
                            f(&DtbReserved { base, size, no_map, memreserve: false, name });
                        }
                        off += step;
                    }
                }
                if depth == 2 { in_rm = false; }
                depth = depth.saturating_sub(1);
                if depth == 0 { break; }
                ptr = ptr.add(4);
            }
            FDT_PROP => {
                let len = be32(ptr.add(4)) as usize;
                let no = be32(ptr.add(8)) as usize;
                let pn = core::slice::from_raw_parts(strings.add(no), strlen(strings.add(no)));
                let d = ptr.add(12);
                match (depth, pn) {
                    (1, b"#address-cells") if len >= 4 => root_ac = be32(d) as usize,
                    (1, b"#size-cells") if len >= 4 => root_sc = be32(d) as usize,
                    (2, b"#address-cells") if in_rm && len >= 4 => rm_ac = be32(d) as usize,
                    (2, b"#size-cells") if in_rm && len >= 4 => rm_sc = be32(d) as usize,
                    (3, b"reg") if in_rm => reg = (d, len),
                    (3, b"no-map") if in_rm => no_map = true,
                    (3, b"status") if in_rm => {
                        let s = core::slice::from_raw_parts(d, len);
                        disabled = !(s.starts_with(b"okay") || s.starts_with(b"ok\0"));
                    }
                    _ => {}
                }
                ptr = ptr.add(12 + align_up(len, 4));
            }
            FDT_NOP => ptr = ptr.add(4),
            _ => break,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;
    use super::*;

    /// Minimal FDT writer: (tokens, strings) assembled into a v17 blob.
    struct W { s: Vec<u8>, st: Vec<u8> }
    impl W {
        fn u32(&mut self, v: u32) { self.s.extend_from_slice(&v.to_be_bytes()); }
        fn pad(&mut self) { while self.s.len() % 4 != 0 { self.s.push(0); } }
        fn begin(&mut self, n: &str) { self.u32(1); self.s.extend_from_slice(n.as_bytes()); self.s.push(0); self.pad(); }
        fn end(&mut self) { self.u32(2); }
        fn prop(&mut self, n: &str, d: &[u8]) {
            let off = self.st.len() as u32;
            self.st.extend_from_slice(n.as_bytes()); self.st.push(0);
            self.u32(3); self.u32(d.len() as u32); self.u32(off);
            self.s.extend_from_slice(d); self.pad();
        }
    }
    fn cells(v: &[u32]) -> Vec<u8> { v.iter().flat_map(|c| c.to_be_bytes()).collect() }

    fn blob(rsv: &[(u64, u64)]) -> Vec<u64> {
        let mut w = W { s: Vec::new(), st: Vec::new() };
        w.begin("");
        w.prop("#address-cells", &cells(&[2]));
        w.prop("#size-cells", &cells(&[2]));
        w.begin("memory@40000000");
        w.prop("reg", &cells(&[0, 0x4000_0000, 0, 0x4000_0000]));
        w.end();
        w.begin("reserved-memory");
        w.prop("#address-cells", &cells(&[2]));
        w.prop("#size-cells", &cells(&[1]));
        w.prop("ranges", &[]);
        w.begin("atf@0");
        w.prop("reg", &cells(&[0, 0x4100_0000, 0x8_0000]));
        w.prop("no-map", &[]);
        w.end();
        w.begin("two@0"); // two reg entries, no no-map
        w.prop("reg", &cells(&[0, 0x4200_0000, 0x1000, 0x1, 0x0, 0x2000]));
        w.end();
        w.begin("off@0");
        w.prop("reg", &cells(&[0, 0x4300_0000, 0x1000]));
        w.prop("status", b"disabled\0");
        w.end();
        w.begin("cma"); // dynamic: no reg
        w.prop("size", &cells(&[0x400_0000]));
        w.begin("nested@0"); // depth 4: ignored
        w.prop("reg", &cells(&[0, 0x4400_0000, 0x1000]));
        w.end();
        w.end();
        w.end();
        w.begin("pcie@10000000");
        w.prop("bus-range", &cells(&[0, 0x3f]));
        w.end();
        w.end();
        w.u32(9);
        let mut m = Vec::new();
        for &(a, s) in rsv.iter().chain([(0u64, 0u64)].iter()) {
            m.extend_from_slice(&a.to_be_bytes()); m.extend_from_slice(&s.to_be_bytes());
        }
        let off_rsv = 40usize;
        let off_struct = off_rsv + m.len();
        let off_str = off_struct + w.s.len();
        let total = off_str + w.st.len();
        let mut b = Vec::new();
        for v in [FDT_MAGIC, total as u32, off_struct as u32, off_str as u32, off_rsv as u32,
                  17, 16, 0, w.st.len() as u32, w.s.len() as u32] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&m); b.extend_from_slice(&w.s); b.extend_from_slice(&w.st);
        while b.len() % 8 != 0 { b.push(0); }
        b.chunks(8).map(|c| u64::from_ne_bytes(c.try_into().unwrap())).collect() // 8-aligned
    }

    #[test]
    fn memreserve_and_reserved_memory() {
        let b = blob(&[(0x4800_0000, 0x10_0000)]);
        let mut got = Vec::new();
        assert!(unsafe { for_each_reserved_at(b.as_ptr() as usize, &mut |r| got.push(*r)) });
        let v: Vec<(u64, u64, bool, bool, &str)> =
            got.iter().map(|r| (r.base, r.size, r.no_map, r.memreserve, r.name_str())).collect();
        assert_eq!(v, std::vec![
            (0x4800_0000, 0x10_0000, false, true, ""),
            (0x4100_0000, 0x8_0000, true, false, "atf@0"),
            (0x4200_0000, 0x1000, false, false, "two@0"),
            (0x1_0000_0000, 0x2000, false, false, "two@0"),
        ]);
    }

    #[test]
    fn parse_records_reserved_and_bus_range() {
        let b = blob(&[]);
        let info = unsafe { parse(b.as_ptr() as usize) };
        assert_eq!(info.memory_map_len, 1);
        let mut n = 0;
        reserved_ranges(&mut |r| { n += 1; assert!(!r.memreserve); });
        assert_eq!(n, 3);
        assert_eq!(pci_bus_range(), Some((0, 0x3f)));
    }

    #[test]
    fn no_reserved_memory_node() {
        let mut w = W { s: Vec::new(), st: Vec::new() };
        w.begin(""); w.end(); w.u32(9);
        let _ = w; // a garbage pointer must be rejected, not walked
        assert!(!unsafe { for_each_reserved_at(0, &mut |_| panic!()) });
    }
}
