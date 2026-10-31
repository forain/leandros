//! kapi exports: PCI, memory, entropy
use super::*;

// ── PCI / memory / entropy exports (L5a) ────────────────────────────────────

use kmod_abi::{KPciDev, KMAP_MMIO, KMAP_WC, KMAP_WRITABLE};

pub(super) fn unbdf(bdf: u32) -> (u8, u8, u8) { ((bdf >> 16) as u8, (bdf >> 8) as u8, bdf as u8) }

/// Index-th PCI function matching vendor/device (0xffff = any device). 0 ok, -19 none, -22 bad out.
#[no_mangle]
pub extern "C" fn kapi_pci_find(vendor: u16, device: u16, index: u32, out: *mut KPciDev) -> i32 {
    if out.is_null() { return -22; }
    let hit = drivers::pci::scan().into_iter()
        .filter(|d| d.vendor_id == vendor && (device == 0xffff || d.device_id == device))
        .nth(index as usize);
    match hit {
        Some(d) => {
            unsafe {
                out.write(KPciDev {
                    bdf: (d.bus as u32) << 16 | (d.dev as u32) << 8 | d.func as u32,
                    vendor: d.vendor_id,
                    device: d.device_id,
                    class: (d.class as u32) << 16 | (d.subclass as u32) << 8 | d.prog_if as u32,
                    _pad: 0,
                    bars: d.bars,
                });
            }
            0
        }
        None => -19,
    }
}

/// Config read of `width` (1, 2 or 4) bytes at `off` (< 256). Bad arguments read as all-ones.
#[no_mangle]
pub extern "C" fn kapi_pci_cfg_read(bdf: u32, off: u16, width: u8) -> u32 {
    let (b, d, f) = unbdf(bdf);
    if off > 0xff { return u32::MAX; }
    let o = off as u8;
    unsafe {
        match width {
            1 => drivers::pci::pci_read_config_8(b, d, f, o) as u32,
            2 => drivers::pci::pci_read_config_16(b, d, f, o) as u32,
            4 => drivers::pci::pci_read_config_32(b, d, f, o),
            _ => u32::MAX,
        }
    }
}

#[no_mangle]
pub extern "C" fn kapi_pci_cfg_write(bdf: u32, off: u16, width: u8, val: u32) {
    let (b, d, f) = unbdf(bdf);
    if off > 0xff { return; }
    crate::kmod::lower::note_pci(bdf);
    let o = off as u8;
    unsafe {
        match width {
            1 => drivers::pci::pci_write_config_8(b, d, f, o, val as u8),
            2 => drivers::pci::pci_write_config_16(b, d, f, o, val as u16),
            4 => drivers::pci::pci_write_config_32(b, d, f, o, val),
            _ => {}
        }
    }
}

/// Map device memory into the kernel. Only KMAP_* are honoured: the mapping is always
/// kernel-only and non-executable. Returns 0 on failure (the caller falls back itself).
#[no_mangle]
pub extern "C" fn kapi_map_device(phys: u64, len: usize, flags: u32) -> usize {
    use mm::paging::{map_kernel_device, PageFlags};
    if len == 0 { return 0; }
    // UC: a strongly-ordered kernel-VA mapping (ioremap), the mode register BARs need.
    if flags & kmod_abi::KMAP_UC != 0 {
        return mm::kva::ioremap(phys, len, mm::kva::CacheMode::Uncached).unwrap_or(0);
    }
    let mut f = PageFlags::PRESENT;
    if flags & KMAP_WRITABLE != 0 { f |= PageFlags::WRITABLE; }
    if flags & KMAP_MMIO != 0 { f |= PageFlags::MMIO; }
    if flags & KMAP_WC != 0 { f |= PageFlags::WRITECOMBINE; }
    unsafe { map_kernel_device(phys as usize, len, f).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn kapi_phys_to_virt(phys: u64) -> usize { mm::phys_to_virt(phys as usize) }

/// 2^order pages. `below` is an exclusive physical ceiling (0 = any); the block ends at or below it.
#[no_mangle]
pub extern "C" fn kapi_page_alloc(order: u32, below: u64) -> u64 {
    if order as usize >= mm::buddy::MAX_ORDER { return 0; }
    let r = if below == 0 { mm::buddy::alloc(order as usize) }
            else { mm::buddy::alloc_below(order as usize, below as usize) };
    let p = r.map_or(0, |p| p as u64);
    crate::kmod::lower::note_page_alloc(p, order);
    p
}

#[no_mangle]
pub extern "C" fn kapi_page_free(phys: u64, order: u32) {
    crate::kmod::lower::note_page_free(phys);
    if phys != 0 && (order as usize) < mm::buddy::MAX_ORDER { mm::buddy::free(phys as usize, order as usize); }
}

pub(super) const HW_SLOTS: usize = 4;
type HwRead = extern "C" fn(*mut u8, usize) -> usize;
type HwSlot = spin::Mutex<Option<Arc<crate::kmod::gate::ModFn<HwRead>>>>;
pub(super) static HW_READ: [HwSlot; HW_SLOTS] = [const { spin::Mutex::new(None) }; HW_SLOTS];
/// Slot allocation bitmap (bit i = slot i in use); cleared again on unregister.
pub(super) static HW_USED: AtomicUsize = AtomicUsize::new(0);
/// Per-slot owned copy of the source name (leaked once per slot, reused when equal).
static HW_NAME: [spin::Mutex<Option<&'static str>>; HW_SLOTS] = [const { spin::Mutex::new(None) }; HW_SLOTS];

pub(super) fn hw_tramp<const N: usize>(out: &mut [u8]) -> usize {
    let f = match HW_READ[N].lock().as_ref() { Some(f) => f.clone(), None => return 0 };
    f.with(|p| p(out.as_mut_ptr(), out.len())).unwrap_or(0).min(out.len())
}

/// Register a module entropy source and reseed the CSPRNG at once so it contributes
/// immediately. 0 ok, -12 no free slot (MAX 4), -22 bad name.
#[no_mangle]
pub extern "C" fn kapi_hwrng_register(owner: KmodId, name: *const u8, len: usize,
                                      read: extern "C" fn(*mut u8, usize) -> usize) -> i32 {
    let nm = match core::str::from_utf8(unsafe { bytes(name, len.min(32)) }) {
        Ok(s) if !s.is_empty() => s,
        _ => return -22,
    };
    let owner = crate::kmod::gate::owner(owner);
    let Some(g) = crate::kmod::gate::get(owner) else { return -19 };
    let i = loop {
        let m = HW_USED.load(Ordering::SeqCst);
        let Some(i) = (0..HW_SLOTS).find(|i| m & (1 << i) == 0) else { return -12 };
        if HW_USED.compare_exchange(m, m | (1 << i), Ordering::SeqCst, Ordering::SeqCst).is_ok() { break i; }
    };
    let nm: &'static str = {
        let mut n = HW_NAME[i].lock();
        match *n { Some(s) if s == nm => s, _ => { let s: &'static str = Box::leak(String::from(nm).into_boxed_str()); *n = Some(s); s } }
    };
    *HW_READ[i].lock() = Some(Arc::new(crate::kmod::gate::ModFn::new(g, crate::kmod::gate::Class::Lower, read)));
    let tramp: sched::random::SourceFn = match i {
        0 => hw_tramp::<0>, 1 => hw_tramp::<1>, 2 => hw_tramp::<2>, _ => hw_tramp::<3>,
    };
    sched::random::register_source(nm, tramp);
    crate::kmod::registry::note_registration(owner, RegKind::HWRNG, i as u64);
    sched::random::reseed_now();
    0
}

/// Unload: take the source out of the CSPRNG (waits for a gather in flight via its lock), free the slot.
pub fn hwrng_release(slot: u64) -> bool {
    let i = slot as usize;
    if i >= HW_SLOTS { return false; }
    if let Some(n) = *HW_NAME[i].lock() { sched::random::unregister_source(n); }
    *HW_READ[i].lock() = None;
    HW_USED.fetch_and(!(1 << i), Ordering::SeqCst);
    true
}


