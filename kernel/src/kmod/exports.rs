//! Core kapi v1: the symbols a module may import. Each is `#[no_mangle] extern "C"`
//! and listed in `.ksymtab` (kept by the linker scripts, bounded by
//! `__ksymtab_start/_end`). Subsystem exports are appended by their lanes.

use core::alloc::Layout;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize, Ordering};
use kmod_abi::{sys, KSym, KmodCtx, KmodId, RegKind, KMOD_ABI_VERSION};

fn log(s: &str) { drivers::pci::serial_debug(s); }

unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 { &[] } else { core::slice::from_raw_parts(p, n) }
}

#[no_mangle]
pub extern "C" fn kapi_abi_version() -> u32 { KMOD_ABI_VERSION }

#[no_mangle]
pub extern "C" fn kapi_log(level: u32, ptr: *const u8, len: usize) {
    let m = unsafe { bytes(ptr, len.min(512)) };
    let o = super::load::CUR_OWNER.load(Ordering::SeqCst);
    let nm = if o != 0 { super::registry::name_of(KmodId(o)) } else { [0; 32] };
    let nm = super::name_str(&nm);
    let sep = if nm.is_empty() { "" } else { " " };
    log(&alloc::format!("[kmod{}{}] <{}> {}\n", sep, nm, level, alloc::string::String::from_utf8_lossy(m)));
}

#[no_mangle]
pub extern "C" fn kapi_panic(owner: KmodId, ptr: *const u8, len: usize) -> ! {
    let m = unsafe { bytes(ptr, len.min(512)) };
    let nm = super::registry::name_of(owner);
    panic!("kmod {} (id {}) panic: {}", super::name_str(&nm), owner.0, alloc::string::String::from_utf8_lossy(m));
}

#[no_mangle]
pub extern "C" fn kapi_alloc(size: usize, align: usize) -> *mut u8 {
    match Layout::from_size_align(size, align) {
        Ok(l) if size != 0 => unsafe { alloc::alloc::alloc(l) },
        _ => core::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn kapi_dealloc(ptr: *mut u8, size: usize, align: usize) {
    if let Ok(l) = Layout::from_size_align(size, align) {
        if !ptr.is_null() && size != 0 { unsafe { alloc::alloc::dealloc(ptr, l) } }
    }
}

#[no_mangle]
pub extern "C" fn kapi_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8 {
    match Layout::from_size_align(size, align) {
        Ok(l) if !ptr.is_null() && size != 0 && new_size != 0 => unsafe { alloc::alloc::realloc(ptr, l, new_size) },
        _ => core::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn kapi_cmdline(len_out: *mut usize) -> *const u8 {
    let c = crate::boot_cmdline().unwrap_or("");
    if !len_out.is_null() { unsafe { len_out.write(c.len()); } }
    c.as_ptr()
}

#[no_mangle]
pub extern "C" fn kapi_monotonic_ns() -> u64 { sched::monotonic_ns() }
#[no_mangle]
pub extern "C" fn kapi_msleep(ms: u64) { sched::kthread::msleep(ms) }
#[no_mangle]
pub extern "C" fn kapi_udelay(us: u64) { sched::kthread::udelay(us) }
#[no_mangle]
pub extern "C" fn kapi_spin_held(acquired: bool) { sched::spinheld::account(acquired) }
#[no_mangle]
pub extern "C" fn kapi_spin_held_count() -> u32 { sched::spinheld::held() }

#[no_mangle]
pub extern "C" fn kapi_kthread_spawn(name: *const u8, name_len: usize, f: extern "C" fn(*mut u8),
                                     arg: *mut u8, owner: KmodId) -> i64 {
    let nm = core::str::from_utf8(unsafe { bytes(name, name_len.min(24)) }).unwrap_or("kmod");
    let a = arg as usize;
    match sched::kthread::spawn(nm, move || f(a as *mut u8)) {
        Some(t) => {
            let pid = t.pid();
            super::registry::note_registration(owner, RegKind::KTHREAD, pid as u64);
            pid as i64
        }
        None => -12,
    }
}

static PING_COUNT: AtomicU32 = AtomicU32::new(0);
static PING_OWNER: AtomicU16 = AtomicU16::new(0);
static PING_CTXSZ: AtomicU32 = AtomicU32::new(0);
static PING_ABI: AtomicU32 = AtomicU32::new(0);
static PING_CMDLEN: AtomicUsize = AtomicUsize::new(0);

/// Test aid: counts calls, remembers the last ctx, returns 42 + owner.
#[no_mangle]
pub extern "C" fn kapi_ktest_ping(ctx: *const KmodCtx) -> i32 {
    PING_COUNT.fetch_add(1, Ordering::SeqCst);
    if ctx.is_null() { return -22; }
    let c = unsafe { &*ctx };
    PING_OWNER.store(c.owner.0, Ordering::SeqCst);
    PING_CTXSZ.store(c.ctx_size, Ordering::SeqCst);
    PING_ABI.store(c.abi_version, Ordering::SeqCst);
    PING_CMDLEN.store(c.cmdline_len, Ordering::SeqCst);
    42 + c.owner.0 as i32
}

/// (calls, last owner, last ctx_size, last abi_version, last cmdline_len)
pub fn ping_state() -> (u32, u16, u32, u32, usize) {
    (PING_COUNT.load(Ordering::SeqCst), PING_OWNER.load(Ordering::SeqCst), PING_CTXSZ.load(Ordering::SeqCst),
     PING_ABI.load(Ordering::SeqCst), PING_CMDLEN.load(Ordering::SeqCst))
}

// ── PCI / memory / entropy exports (L5a) ────────────────────────────────────

use kmod_abi::{KPciDev, KMAP_MMIO, KMAP_WC, KMAP_WRITABLE};

fn unbdf(bdf: u32) -> (u8, u8, u8) { ((bdf >> 16) as u8, (bdf >> 8) as u8, bdf as u8) }

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
    if order > 10 { return 0; }
    let r = if below == 0 { mm::buddy::alloc(order as usize) }
            else { mm::buddy::alloc_below(order as usize, below as usize) };
    r.map_or(0, |p| p as u64)
}

#[no_mangle]
pub extern "C" fn kapi_page_free(phys: u64, order: u32) {
    if phys != 0 && order <= 10 { mm::buddy::free(phys as usize, order as usize); }
}

const HW_SLOTS: usize = 4;
static HW_READ: [AtomicUsize; HW_SLOTS] = [const { AtomicUsize::new(0) }; HW_SLOTS];
static HW_USED: AtomicUsize = AtomicUsize::new(0);

fn hw_tramp<const N: usize>(out: &mut [u8]) -> usize {
    let p = HW_READ[N].load(Ordering::Acquire);
    if p == 0 { return 0; }
    let f: extern "C" fn(*mut u8, usize) -> usize = unsafe { core::mem::transmute(p) };
    f(out.as_mut_ptr(), out.len()).min(out.len())
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
    let i = HW_USED.fetch_add(1, Ordering::SeqCst);
    if i >= HW_SLOTS { HW_USED.store(HW_SLOTS, Ordering::SeqCst); return -12; }
    // Modules are permanent, so the name bytes live as long as the kernel.
    let nm: &'static str = unsafe { core::mem::transmute::<&str, &'static str>(nm) };
    HW_READ[i].store(read as usize, Ordering::Release);
    let tramp: sched::random::SourceFn = match i {
        0 => hw_tramp::<0>, 1 => hw_tramp::<1>, 2 => hw_tramp::<2>, _ => hw_tramp::<3>,
    };
    sched::random::register_source(nm, tramp);
    super::registry::note_registration(owner, RegKind::HWRNG, i as u64);
    sched::random::reseed_now();
    0
}


// ── e1000e-class exports (L6a): PCI enable/caps, IRQs, wait queue, netdev, pm hooks ──────

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::AtomicU64;
use kmod_abi::{KIrqHandler, KNetifOps, KPmFn, KNET_WIFI};

fn func_of(bdf: u32) -> drivers::pci::PciFunction {
    let (b, d, f) = unbdf(bdf);
    drivers::pci::PciFunction::new(b, d, f)
}

/// Memory decode on, bus mastering on when asked. 0 ok, -19 no such function.
#[no_mangle]
pub extern "C" fn kapi_pci_enable(bdf: u32, bus_master: bool) -> i32 {
    let f = func_of(bdf);
    if f.read16(0) == 0xffff { return -19; }
    if bus_master { f.enable_mem_and_bus_master(); } else { f.enable_memory(); }
    0
}

/// Config-space offset of capability `cap_id` (PM = 1, PCIe = 0x10, ...), 0 when absent.
#[no_mangle]
pub extern "C" fn kapi_pci_find_cap(bdf: u32, cap_id: u8) -> u16 {
    func_of(bdf).find_capability(cap_id).unwrap_or(0)
}

fn irq_errno(e: drivers::irq::IrqError) -> i64 {
    use drivers::irq::IrqError::*;
    match e { NoSpace | NoMemory => -12, NotFound => -19, Unsupported => -95, Busy => -16 }
}

/// The handler must return 0 (not ours) or 1 (handled): that is `IrqReturn`'s repr.
fn irq_handler(h: KIrqHandler) -> drivers::irq::IrqHandler {
    unsafe { core::mem::transmute::<KIrqHandler, drivers::irq::IrqHandler>(h) }
}

/// Single-vector MSI (x86_64). Returns the vector, or -errno (-95 where MSI is unsupported).
#[no_mangle]
pub extern "C" fn kapi_irq_request_msi(owner: KmodId, bdf: u32, handler: KIrqHandler, ctx: usize) -> i64 {
    match drivers::irq::request_msi(func_of(bdf), irq_handler(handler), ctx) {
        Ok(h) => {
            let v = h.vector().unwrap_or(0) as u64;
            core::mem::forget(h); // modules are permanent
            super::registry::note_registration(owner, RegKind::IRQ, v);
            v as i64
        }
        Err(e) => irq_errno(e),
    }
}

/// The function's INTx line (aarch64 `virt`). Returns the line, or -errno. The caller clears
/// the command register's INTx-disable bit when it wants the line live.
#[no_mangle]
pub extern "C" fn kapi_irq_request_line(owner: KmodId, bdf: u32, handler: KIrqHandler, ctx: usize) -> i64 {
    let Some(line) = func_of(bdf).intx_line() else { return -19 };
    match drivers::irq::request_irq(drivers::irq::IrqSource::Line(line), irq_handler(handler), ctx) {
        Ok(h) => {
            core::mem::forget(h);
            super::registry::note_registration(owner, RegKind::IRQ, line as u64);
            line as i64
        }
        Err(e) => irq_errno(e),
    }
}

/// A wait queue with an event counter: the module's `LinkMon`.
struct KWait {
    wq: sched::ksync::KWaitQueue,
    events: AtomicU64,
    seen: AtomicU64,
}

#[no_mangle]
pub extern "C" fn kapi_wait_new() -> usize {
    Box::into_raw(Box::new(KWait {
        wq: sched::ksync::KWaitQueue::new(), events: AtomicU64::new(0), seen: AtomicU64::new(0),
    })) as usize
}

/// Any context (IRQ included). Handles are permanent.
#[no_mangle]
pub extern "C" fn kapi_wait_notify(h: usize) {
    if h == 0 { return; }
    let w = unsafe { &*(h as *const KWait) };
    w.events.fetch_add(1, Ordering::AcqRel);
    w.wq.wake_all();
}

/// Sleep up to `ns` or until an event newer than the last wait's arrives (one waiter per handle).
/// True when woken by an event.
#[no_mangle]
pub extern "C" fn kapi_wait_timeout(h: usize, ns: u64) -> bool {
    if h == 0 { return false; }
    let w = unsafe { &*(h as *const KWait) };
    let s = w.seen.load(Ordering::Acquire);
    let hit = w.wq.wait_event_timeout(|| w.events.load(Ordering::Acquire) != s, ns);
    w.seen.store(w.events.load(Ordering::Acquire), Ordering::Release);
    hit
}

struct ModNetif { ops: KNetifOps, ctx: usize, name: &'static str }

/// Write-once slots (leaked boxes); a netif's `dev` is its slot index.
static MOD_NETIF: [AtomicUsize; drivers::netdev::MAX_NETIF] = [const { AtomicUsize::new(0) }; drivers::netdev::MAX_NETIF];
static MOD_NETIF_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn mn(d: u8) -> &'static ModNetif { unsafe { &*(MOD_NETIF[d as usize].load(Ordering::Acquire) as *const ModNetif) } }

static MOD_OPS: drivers::netdev::NetifOps = drivers::netdev::NetifOps {
    driver: |d| mn(d).name,
    send: |d, b| { let m = mn(d); (m.ops.send)(m.ctx, b.as_ptr(), b.len()) },
    tx_ready: |d| { let m = mn(d); (m.ops.tx_ready)(m.ctx) },
    recv: |d, b| {
        let m = mn(d);
        let n = (m.ops.recv)(m.ctx, b.as_mut_ptr(), b.len());
        if n < 0 { None } else { Some((n as usize).min(b.len())) }
    },
    rx_rearm: |d| { let m = mn(d); (m.ops.rx_rearm)(m.ctx) },
    rx_irq_driven: |d| { let m = mn(d); (m.ops.rx_irq_driven)(m.ctx) },
    mac: |d| {
        let m = mn(d);
        let mut a = [0u8; 6];
        if (m.ops.mac)(m.ctx, a.as_mut_ptr()) { Some(a) } else { None }
    },
    link: |d| {
        let m = mn(d);
        let (mut up, mut mbps, mut full) = (false, 0u32, false);
        (m.ops.link)(m.ctx, &mut up, &mut mbps, &mut full);
        (up, mbps, full)
    },
    link_gen: |d| { let m = mn(d); (m.ops.link_gen)(m.ctx) },
    info: |d, out| {
        let m = mn(d);
        let mut buf = alloc::vec![0u8; 2048];
        let n = (m.ops.info)(m.ctx, buf.as_mut_ptr(), buf.len()).min(buf.len());
        out.push_str(&String::from_utf8_lossy(&buf[..n]));
    },
};

/// Register a netif from a module, at any time. Returns the netif id, or -errno
/// (-22 bad arguments, -12 no free slot). The net daemon is kicked and builds the stack on its next pass.
#[no_mangle]
pub extern "C" fn kapi_netdev_register(owner: KmodId, kind: u32, prefix: *const u8, prefix_len: usize,
                                       ops: *const KNetifOps, ctx: usize) -> i32 {
    let pfx = match core::str::from_utf8(unsafe { bytes(prefix, prefix_len.min(8)) }) {
        Ok(s) if !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_lowercase()) => s,
        _ => return -22,
    };
    if ops.is_null() { return -22; }
    let ops = unsafe { *ops };
    let mut nb = [0u8; 32];
    let n = (ops.driver_name)(ctx, nb.as_mut_ptr(), nb.len()).min(nb.len());
    let name: &'static str = Box::leak(String::from_utf8_lossy(&nb[..n]).into_owned().into_boxed_str());
    let slot = {
        let _g = MOD_NETIF_LOCK.lock();
        let Some(i) = MOD_NETIF.iter().position(|s| s.load(Ordering::Acquire) == 0) else { return -12 };
        MOD_NETIF[i].store(Box::into_raw(Box::new(ModNetif { ops, ctx, name })) as usize, Ordering::Release);
        i
    };
    let k = if kind == KNET_WIFI { drivers::netdev::Kind::Wifi } else { drivers::netdev::Kind::Wired };
    match drivers::netdev::register_late(k, pfx, &MOD_OPS, slot as u8) {
        Some(id) => {
            super::registry::note_registration(owner, RegKind::NETDEV, id as u64);
            id as i32
        }
        None => -12,
    }
}

#[no_mangle]
pub extern "C" fn kapi_netdev_kick() { drivers::netdev::kick(); }
#[no_mangle]
pub extern "C" fn kapi_netdev_kick_from_irq() { drivers::netdev::kick_from_irq(); }

#[derive(Clone, Copy)]
struct PmHooks { suspend: Option<KPmFn>, resume: Option<KPmFn>, shutdown: Option<KPmFn>, ctx: usize }
static PM_HOOKS: spin::Mutex<Option<PmHooks>> = spin::Mutex::new(None);

/// Register the NIC's power-management hooks (one set; the builtin e1000e is then a stub).
/// 0 ok, -16 already registered.
#[no_mangle]
pub extern "C" fn kapi_pm_register(_owner: KmodId, suspend: Option<KPmFn>, resume: Option<KPmFn>,
                                   shutdown: Option<KPmFn>, ctx: usize) -> i32 {
    let mut g = PM_HOOKS.lock();
    if g.is_some() { return -16; }
    *g = Some(PmHooks { suspend, resume, shutdown, ctx });
    0
}

// ── uvc-class exports (L7a): V4L2 camera registration, USB class drivers, tick callbacks ──

use kmod_abi::{KCameraOps, KTickFn, KUsbDetachFn, KUsbIf, KUsbProbeFn, KmodDevId, BUS_USB};

/// Register a camera with the V4L2 layer; it appears as /dev/video0 candidate at once.
/// 0 ok, -22 bad table or description.
#[no_mangle]
pub extern "C" fn kapi_v4l2_register(owner: KmodId, ops: *const KCameraOps, ctx: usize) -> i32 {
    if ops.is_null() { return -22; }
    // Modules are permanent, so the table lives as long as the kernel.
    let ops: &'static KCameraOps = unsafe { &*ops };
    let r = crate::syscall::v4l2::register_camera(owner, ops, ctx);
    if r == 0 { super::registry::note_registration(owner, RegKind::V4L2, ctx as u64); }
    r
}

fn tick_tramp(p: usize) {
    let b = unsafe { &*(p as *const (KTickFn, usize)) };
    (b.0)(b.1);
}

/// Run `f(ctx)` on every USB timer tick (interrupt context, must not block). 0 ok.
#[no_mangle]
pub extern "C" fn kapi_tick_register(owner: KmodId, f: KTickFn, ctx: usize) -> i32 {
    let p = Box::into_raw(Box::new((f, ctx))) as usize;
    usb::class::tick_register(owner, tick_tramp, p);
    0
}

// ── USB function and isochronous handle tables ──
//
// A module never sees an xhci type. A claimed interface is a `u64` handle into FUNCS; an open
// isochronous endpoint is a `u64` handle into PIPES. Handles count up and are never reused, so a
// stale one fails with -19 instead of hitting somebody else's function. Locks are never held across
// a call into xhci or into a module.

use usb::xhci::isoch::{IsochPipe, Poll as IsoPoll, StreamFormat, UsbFunction};

/// (handle, owning driver's registration pointer, function)
static FUNCS: spin::Mutex<alloc::vec::Vec<(u64, usize, Arc<UsbFunction>)>> = spin::Mutex::new(alloc::vec::Vec::new());
/// (pipe handle, function handle, `*mut IsochPipe`). The module serialises use of one pipe.
static PIPES: spin::Mutex<alloc::vec::Vec<(u64, u64, usize)>> = spin::Mutex::new(alloc::vec::Vec::new());
static NEXT_HANDLE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Number of USB functions held by module class drivers (ktest).
pub fn usb_claimed() -> usize { FUNCS.lock().len() }
/// Number of open isochronous pipes held by modules (ktest).
pub fn usb_pipes() -> usize { PIPES.lock().len() }

fn func(h: u64) -> Option<Arc<UsbFunction>> { FUNCS.lock().iter().find(|e| e.0 == h).map(|e| e.2.clone()) }

fn usb_errno(e: usb::UsbErr) -> i32 {
    match e {
        usb::UsbErr::Stall => -32,
        usb::UsbErr::Timeout => -110,
        usb::UsbErr::Babble => -75,
        usb::UsbErr::Disconnected => -19,
        usb::UsbErr::Other(_) => -5,
    }
}

/// Forget function `h` and close the pipes still open on it.
fn drop_func(h: u64) {
    FUNCS.lock().retain(|e| e.0 != h);
    let mut dead = alloc::vec::Vec::new();
    PIPES.lock().retain(|e| { if e.1 == h { dead.push(e.2); false } else { true } });
    for p in dead { unsafe { Box::from_raw(p as *mut IsochPipe) }.close(); }
}

/// USB control transfer on the claimed function. The direction is bit 7 of `setup.request_type`;
/// `buf` must hold `setup.length` bytes. Returns the bytes transferred (IN) / 0 (OUT), or -errno.
/// Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_ctrl(handle: u64, setup: *const kmod_abi::KUsbSetup, buf: *mut u8, len: usize,
                                timeout_ms: u32) -> i32 {
    if setup.is_null() { return -22; }
    let s = unsafe { *setup };
    let n = s.length as usize;
    if n > len || (n != 0 && buf.is_null()) { return -22; }
    let Some(f) = func(handle) else { return -19 };
    let to = if timeout_ms == 0 { 5000 } else { timeout_ms };
    let (rt, rq, v, i) = (s.request_type, s.request, s.value, s.index);
    if rt & 0x80 != 0 {
        let b: &mut [u8] = if n == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf, n) } };
        match f.control_in_t(rt, rq, v, i, b, to) { Ok(k) => k as i32, Err(e) => usb_errno(e) }
    } else {
        let b: &[u8] = if n == 0 { &[] } else { unsafe { core::slice::from_raw_parts(buf, n) } };
        match f.control_out_t(rt, rq, v, i, b, to) { Ok(()) => 0, Err(e) => usb_errno(e) }
    }
}

/// SET_INTERFACE on a claimed function. 0 or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_set_alt(handle: u64, iface: u8, alt: u8) -> i32 {
    let Some(f) = func(handle) else { return -19 };
    match f.set_interface(iface, alt) { Ok(()) => 0, Err(e) => usb_errno(e) }
}

/// Copy the function's configuration descriptor into `buf` (up to `len` bytes). Returns the full
/// descriptor length (call with `len` 0 to size it), or -errno.
#[no_mangle]
pub extern "C" fn kapi_usb_cfg(handle: u64, buf: *mut u8, len: usize) -> i32 {
    let Some(f) = func(handle) else { return -19 };
    let k = f.cfg.len().min(len);
    if k != 0 {
        if buf.is_null() { return -22; }
        unsafe { core::ptr::copy_nonoverlapping(f.cfg.as_ptr(), buf, k); }
    }
    f.cfg.len() as i32
}

/// Open the isochronous endpoint `ep_addr` of alternate setting `alt` of interface `iface` (it must
/// be in the configuration descriptor): bandwidth is configured and SET_INTERFACE is issued. `framed`
/// keeps one FIFO record per service interval (read back with `kapi_usb_isoch_reap`). Returns the pipe
/// handle (> 0) or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_open(handle: u64, iface: u8, alt: u8, ep_addr: u8, fifo_bytes: usize,
                                      ntds: usize, framed: bool) -> i64 {
    let Some(f) = func(handle) else { return -19 };
    let Some(d) = usb::isocdesc::scan_isoch(&f.cfg).into_iter()
        .find(|d| d.iface == iface && d.alt == alt && d.ep_addr == ep_addr) else { return -22 };
    match IsochPipe::open_ex(&f, &d, StreamFormat { rate: 0, frame_bytes: 0 }, fifo_bytes, ntds, framed) {
        Ok(p) => {
            let ph = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let ptr = Box::into_raw(Box::new(p)) as usize;
            PIPES.lock().push((ph, handle, ptr));
            ph as i64
        }
        Err(_) => -5,
    }
}

fn pipe_ptr(ph: u64) -> Option<*mut IsochPipe> {
    PIPES.try_lock()?.iter().find(|e| e.0 == ph).map(|e| e.2 as *mut IsochPipe)
}

/// Retire completed transfers and keep the endpoint's ring full (IRQ-safe: never blocks).
/// 0 ok, 1 controller busy (try next tick), 2 device gone (reported once), -19 bad handle.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_submit(pipe: u64) -> i32 {
    let Some(p) = pipe_ptr(pipe) else { return 1 };
    match unsafe { (*p).poll() } { IsoPoll::Ok => 0, IsoPoll::Busy => 1, IsoPoll::Gone => 2 }
}

/// Take the oldest framed packet. Returns its length (it may exceed `len`; the excess is dropped),
/// -11 when none is queued, -19 for a bad handle. Never blocks.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_reap(pipe: u64, buf: *mut u8, len: usize) -> i32 {
    let Some(p) = pipe_ptr(pipe) else { return -11 };
    if buf.is_null() && len != 0 { return -22; }
    let out: &mut [u8] = if len == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf, len) } };
    match unsafe { (*p).read_packet(out) } { Some(n) => n as i32, None => -11 }
}

/// Stop the endpoint, select alternate setting 0 and free the pipe. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_close(pipe: u64) {
    let mut found = None;
    PIPES.lock().retain(|e| { if e.0 == pipe { found = Some(e.2); false } else { true } });
    if let Some(p) = found { unsafe { Box::from_raw(p as *mut IsochPipe) }.close(); }
}

/// Remove a camera registered with `kapi_v4l2_register` (the device went away).
#[no_mangle]
pub extern "C" fn kapi_v4l2_unregister(ctx: usize) { crate::syscall::v4l2::unregister_camera(ctx); }

/// What a registered class driver's `ctx` points at.
struct UsbCls { probe: KUsbProbeFn, detach: Option<KUsbDetachFn>, ctx: usize }

fn usb_probe_tramp(f: UsbFunction, m: &usb::class::Match, p: usize) -> Result<(), UsbFunction> {
    let b = unsafe { &*(p as *const UsbCls) };
    let h = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let f = Arc::new(f);
    // The probe talks to the device through the handle, so it must exist before the call.
    FUNCS.lock().push((h, p, f.clone()));
    let intf = KUsbIf {
        handle: h, vid: m.vid, pid: m.pid, slot: f.slot,
        class: m.class, subclass: m.subclass, protocol: m.protocol, iface: m.iface, _pad: [0; 7],
    };
    if (b.probe)(&intf, b.ctx) == 0 { return Ok(()); }
    drop_func(h);
    match Arc::try_unwrap(f) { Ok(f) => Err(f), Err(_) => Ok(()) }
}

/// The device on (`hc`, `slot`) left: tell the driver, then drop its handles.
fn usb_detach_tramp(hc: &usb::xhci::Hc, slot: u8, p: usize) {
    let b = unsafe { &*(p as *const UsbCls) };
    let hs: alloc::vec::Vec<u64> = FUNCS.lock().iter()
        .filter(|e| e.1 == p && core::ptr::eq(&*e.2.hc, hc) && e.2.slot == slot).map(|e| e.0).collect();
    for h in hs {
        if let Some(d) = b.detach { d(h, b.ctx); }
        drop_func(h);
    }
}

/// Register a USB class driver for the given USB id-table entries. Interfaces that were
/// enumerated earlier and left unclaimed are offered to it before this returns. `detach` runs when
/// a claimed device leaves the bus. 0 ok, -22 bad arguments.
#[no_mangle]
pub extern "C" fn kapi_usb_class_register(owner: KmodId, ids: *const KmodDevId, nids: usize,
                                          probe: KUsbProbeFn, detach: Option<KUsbDetachFn>, ctx: usize) -> i32 {
    if ids.is_null() || nids == 0 || nids > 64 { return -22; }
    let v: alloc::vec::Vec<usb::class::IdEntry> = unsafe { core::slice::from_raw_parts(ids, nids) }
        .iter().filter(|e| e.bus == BUS_USB)
        .map(|e| usb::class::IdEntry { vid: e.a, pid: e.b, class: e.class, mask: e.class_mask }).collect();
    if v.is_empty() { return -22; }
    let p = Box::into_raw(Box::new(UsbCls { probe, detach, ctx })) as usize;
    super::registry::note_registration(owner, RegKind::USB_CLASS, p as u64);
    usb::class::register(usb::class::Driver {
        owner, ids: usb::class::leak_ids(v), probe: usb_probe_tramp, detach: Some(usb_detach_tramp), ctx: p,
    });
    0
}

fn run_pm(f: Option<KPmFn>, ctx: usize) -> Option<(i32, String)> {
    let f = f?;
    let mut buf = [0u8; 256];
    let r = f(ctx, buf.as_mut_ptr(), buf.len());
    let n = (r.unsigned_abs() as usize).min(buf.len());
    Some((r, String::from(String::from_utf8_lossy(&buf[..n]))))
}

fn pm_result(h: Option<PmHooks>, pick: fn(&PmHooks) -> Option<KPmFn>) -> Option<Result<String, String>> {
    let h = h?;
    let (r, m) = run_pm(pick(&h), h.ctx)?;
    Some(if r >= 0 { Ok(m) } else { Err(m) })
}
fn pm_hooks() -> Option<PmHooks> { *PM_HOOKS.lock() }

/// `Some` only when a module registered hooks; pm.rs falls back to the builtin otherwise.
pub fn pm_suspend() -> Option<Result<String, String>> { pm_result(pm_hooks(), |h| h.suspend) }
pub fn pm_resume() -> Option<Result<String, String>> { pm_result(pm_hooks(), |h| h.resume) }
pub fn pm_shutdown() -> Option<String> {
    match pm_result(pm_hooks(), |h| h.shutdown)? { Ok(m) if !m.is_empty() => Some(m), Err(m) => Some(m), _ => None }
}

// Compile-time signature check against kmod_abi::sys.
const _: sys::AbiVersionFn = kapi_abi_version;
const _: sys::LogFn = kapi_log;
const _: sys::PanicFn = kapi_panic;
const _: sys::AllocFn = kapi_alloc;
const _: sys::DeallocFn = kapi_dealloc;
const _: sys::ReallocFn = kapi_realloc;
const _: sys::CmdlineFn = kapi_cmdline;
const _: sys::MonotonicFn = kapi_monotonic_ns;
const _: sys::SleepFn = kapi_msleep;
const _: sys::SleepFn = kapi_udelay;
const _: sys::SpinHeldFn = kapi_spin_held;
const _: sys::SpinHeldCountFn = kapi_spin_held_count;
const _: sys::KthreadSpawnFn = kapi_kthread_spawn;
const _: sys::KtestPingFn = kapi_ktest_ping;
const _: sys::PciFindFn = kapi_pci_find;
const _: sys::PciCfgReadFn = kapi_pci_cfg_read;
const _: sys::PciCfgWriteFn = kapi_pci_cfg_write;
const _: sys::MapDeviceFn = kapi_map_device;
const _: sys::PhysToVirtFn = kapi_phys_to_virt;
const _: sys::PageAllocFn = kapi_page_alloc;
const _: sys::PageFreeFn = kapi_page_free;
const _: sys::HwrngRegisterFn = kapi_hwrng_register;
const _: sys::PciEnableFn = kapi_pci_enable;
const _: sys::PciFindCapFn = kapi_pci_find_cap;
const _: sys::IrqRequestMsiFn = kapi_irq_request_msi;
const _: sys::IrqRequestLineFn = kapi_irq_request_line;
const _: sys::WaitNewFn = kapi_wait_new;
const _: sys::WaitTimeoutFn = kapi_wait_timeout;
const _: sys::WaitNotifyFn = kapi_wait_notify;
const _: sys::NetdevRegisterFn = kapi_netdev_register;
const _: sys::NetdevKickFn = kapi_netdev_kick;
const _: sys::NetdevKickFn = kapi_netdev_kick_from_irq;
const _: sys::PmRegisterFn = kapi_pm_register;
const _: sys::V4l2RegisterFn = kapi_v4l2_register;
const _: sys::TickRegisterFn = kapi_tick_register;
const _: sys::UsbClassRegisterFn = kapi_usb_class_register;
const _: sys::UsbCtrlFn = kapi_usb_ctrl;
const _: sys::UsbSetAltFn = kapi_usb_set_alt;
const _: sys::UsbCfgFn = kapi_usb_cfg;
const _: sys::UsbIsochOpenFn = kapi_usb_isoch_open;
const _: sys::UsbIsochSubmitFn = kapi_usb_isoch_submit;
const _: sys::UsbIsochReapFn = kapi_usb_isoch_reap;
const _: sys::UsbIsochCloseFn = kapi_usb_isoch_close;
const _: sys::V4l2UnregisterFn = kapi_v4l2_unregister;
const _: () = {
    assert!(mm::kmodwin::BASE as u64 == kmod_abi::layout::BASE);
    assert!(mm::kmodwin::SIZE as u64 == kmod_abi::layout::SIZE);
};

macro_rules! sym { ($n:literal, $f:ident) => { KSym { name: concat!($n, "\0").as_ptr(), addr: $f as *const () } }; }

pub const NEXPORTS: usize = 44;

#[used]
#[link_section = ".ksymtab"]
static KSYMTAB: [KSym; NEXPORTS] = [
    sym!("kapi_abi_version", kapi_abi_version),
    sym!("kapi_log", kapi_log),
    sym!("kapi_panic", kapi_panic),
    sym!("kapi_alloc", kapi_alloc),
    sym!("kapi_dealloc", kapi_dealloc),
    sym!("kapi_realloc", kapi_realloc),
    sym!("kapi_cmdline", kapi_cmdline),
    sym!("kapi_monotonic_ns", kapi_monotonic_ns),
    sym!("kapi_msleep", kapi_msleep),
    sym!("kapi_udelay", kapi_udelay),
    sym!("kapi_spin_held", kapi_spin_held),
    sym!("kapi_spin_held_count", kapi_spin_held_count),
    sym!("kapi_kthread_spawn", kapi_kthread_spawn),
    sym!("kapi_ktest_ping", kapi_ktest_ping),
    sym!("kapi_pci_find", kapi_pci_find),
    sym!("kapi_pci_cfg_read", kapi_pci_cfg_read),
    sym!("kapi_pci_cfg_write", kapi_pci_cfg_write),
    sym!("kapi_map_device", kapi_map_device),
    sym!("kapi_phys_to_virt", kapi_phys_to_virt),
    sym!("kapi_page_alloc", kapi_page_alloc),
    sym!("kapi_page_free", kapi_page_free),
    sym!("kapi_hwrng_register", kapi_hwrng_register),
    sym!("kapi_pci_enable", kapi_pci_enable),
    sym!("kapi_pci_find_cap", kapi_pci_find_cap),
    sym!("kapi_irq_request_msi", kapi_irq_request_msi),
    sym!("kapi_irq_request_line", kapi_irq_request_line),
    sym!("kapi_wait_new", kapi_wait_new),
    sym!("kapi_wait_timeout", kapi_wait_timeout),
    sym!("kapi_wait_notify", kapi_wait_notify),
    sym!("kapi_netdev_register", kapi_netdev_register),
    sym!("kapi_netdev_kick", kapi_netdev_kick),
    sym!("kapi_netdev_kick_from_irq", kapi_netdev_kick_from_irq),
    sym!("kapi_pm_register", kapi_pm_register),
    sym!("kapi_v4l2_register", kapi_v4l2_register),
    sym!("kapi_tick_register", kapi_tick_register),
    sym!("kapi_usb_class_register", kapi_usb_class_register),
    sym!("kapi_usb_ctrl", kapi_usb_ctrl),
    sym!("kapi_usb_set_alt", kapi_usb_set_alt),
    sym!("kapi_usb_cfg", kapi_usb_cfg),
    sym!("kapi_usb_isoch_open", kapi_usb_isoch_open),
    sym!("kapi_usb_isoch_submit", kapi_usb_isoch_submit),
    sym!("kapi_usb_isoch_reap", kapi_usb_isoch_reap),
    sym!("kapi_usb_isoch_close", kapi_usb_isoch_close),
    sym!("kapi_v4l2_unregister", kapi_v4l2_unregister),
];

extern "C" {
    static __ksymtab_start: KSym;
    static __ksymtab_end: KSym;
}

/// The linked export table, as the linker script delimits it.
pub fn ksymtab() -> &'static [KSym] {
    unsafe {
        let s = core::ptr::addr_of!(__ksymtab_start);
        let e = core::ptr::addr_of!(__ksymtab_end);
        core::slice::from_raw_parts(s, (e as usize - s as usize) / core::mem::size_of::<KSym>())
    }
}

/// Address of an export, found by walking the table.
pub fn lookup(name: &str) -> Option<usize> {
    for k in ksymtab() {
        let mut n = 0;
        while unsafe { *k.name.add(n) } != 0 { n += 1; }
        if unsafe { core::slice::from_raw_parts(k.name, n) } == name.as_bytes() { return Some(k.addr as usize); }
    }
    None
}
