//! kapi exports shared by several driver classes (kmodW2 W2-1). Add-only.
use super::*;

/// Physical address of a DMA-able kernel virtual address. `u64::MAX` for anything outside the
/// direct map: module statics and bss live in the module window and cannot be DMA targets.
#[no_mangle]
pub extern "C" fn kapi_virt_to_phys(va: usize) -> u64 {
    let base = mm::mm_get_hhdm_offset() as usize;
    if va < base || va - base >= (1usize << 44) { return u64::MAX; }
    mm::virt_to_phys(va) as u64
}

/// True when the caller may block.
#[no_mangle]
pub extern "C" fn kapi_can_sleep() -> bool { sched::irqctx::can_sleep() }

/// Sleep until the monotonic clock reaches `ns`.
#[no_mangle]
pub extern "C" fn kapi_sleep_until(ns: u64) { sched::kthread::sleep_until(ns) }

#[no_mangle]
pub extern "C" fn kapi_yield() { sched::kthread::yield_now() }

#[no_mangle]
pub extern "C" fn kapi_irqs_masked() -> bool { sched::irqs_masked() }

#[no_mangle]
pub extern "C" fn kapi_current_pid() -> u32 { sched::current_pid() as u32 }

// ── kmodW2 W2-1 session 2: remaining common exports ─────────────────────────

use alloc::vec::Vec;

/// Map device memory. `cache`: 0 uncached, 1 write-combining, 2 write-back. 0 on failure.
#[no_mangle]
pub extern "C" fn kapi_ioremap(phys: u64, len: usize, cache: u32) -> usize {
    use mm::kva::CacheMode;
    let mode = match cache { 0 => CacheMode::Uncached, 1 => CacheMode::WriteCombining, 2 => CacheMode::WriteBack, _ => return 0 };
    mm::kva::ioremap(phys, len, mode).unwrap_or(0)
}

/// Unmap a `kapi_ioremap` mapping (the whole mapping goes; `len` is advisory).
#[no_mangle]
pub extern "C" fn kapi_iounmap(va: usize, _len: usize) { if va != 0 { let _ = mm::kva::iounmap(va); } }

/// Streaming-DMA sync of a direct-map range. `dir`: 0 to device, 1 to CPU, 2 both. Bad ranges are ignored.
/// 3 = raw clean, 4 = raw invalidate, whatever the coherency mode, on a direct-map or `kapi_vmap`/ioremap
/// window address (no-ops off aarch64, where DMA snoops the caches).
#[no_mangle]
pub extern "C" fn kapi_dma_sync(va: usize, len: usize, dir: u32) {
    if len == 0 || dir > 4 { return; }
    if dir >= 3 {
        if kapi_virt_to_phys(va) == u64::MAX && !mm::kva::in_window(va) { return; }
        #[cfg(target_arch = "aarch64")]
        { if dir == 3 { mm::dma_cache::clean_range(va, len); } else { mm::dma_cache::invalidate_range(va, len); } }
        return;
    }
    if kapi_virt_to_phys(va) == u64::MAX { return; }
    if dir != 1 { mm::dma_cache::sync_for_device(va, len); }
    if dir != 0 { mm::dma_cache::sync_for_cpu(va, len); }
}

/// True when device DMA snoops the CPU caches (x86_64, QEMU virt).
#[no_mangle]
pub extern "C" fn kapi_dma_coherent() -> bool { mm::dma_cache::coherent() }

/// Sleeping mutex. The handle is permanent. 0 on failure.
#[no_mangle]
pub extern "C" fn kapi_mutex_new(_name: *const u8, _len: usize) -> usize {
    Box::into_raw(Box::new(sched::ksync::KRawMutex::new())) as usize
}
#[no_mangle]
pub extern "C" fn kapi_mutex_lock(h: usize) { if h != 0 { unsafe { (*(h as *const sched::ksync::KRawMutex)).lock() } } }
#[no_mangle]
pub extern "C" fn kapi_mutex_unlock(h: usize) { if h != 0 { unsafe { (*(h as *const sched::ksync::KRawMutex)).unlock() } } }

#[no_mangle]
pub extern "C" fn kapi_mutex_trylock(h: usize) -> bool { h != 0 && unsafe { (*(h as *const sched::ksync::KRawMutex)).try_lock() } }

/// Counting semaphore with `n` initial permits. Permanent handle.
#[no_mangle]
pub extern "C" fn kapi_sem_new(n: u32) -> usize { Box::into_raw(Box::new(sched::ksync::KSemaphore::new(n as i64))) as usize }
/// True when a permit was taken before `ns` elapsed (0 = poll once).
#[no_mangle]
pub extern "C" fn kapi_sem_down_timeout(h: usize, ns: u64) -> bool {
    if h == 0 { return false; }
    unsafe { (*(h as *const sched::ksync::KSemaphore)).down_timeout(ns) }
}
#[no_mangle]
pub extern "C" fn kapi_sem_up(h: usize) { if h != 0 { unsafe { (*(h as *const sched::ksync::KSemaphore)).up() } } }

/// High-resolution timer. `cb(ctx)` runs in IRQ context when `irq_mode`, else in a worker. 0 on a null callback.
#[no_mangle]
pub extern "C" fn kapi_hrtimer_new(cb: Option<extern "C" fn(usize)>, ctx: usize, irq_mode: bool) -> usize {
    let Some(cb) = cb else { return 0 };
    let mode = if irq_mode { sched::hrtimer::TimerMode::Irq } else { sched::hrtimer::TimerMode::Worker };
    Box::into_raw(sched::hrtimer::HrTimer::new(cb, ctx, mode)) as usize
}
#[no_mangle]
pub extern "C" fn kapi_hrtimer_arm(h: usize, ns: u64) { if h != 0 { unsafe { (*(h as *const sched::hrtimer::HrTimer)).start(ns, 0); } } }
#[no_mangle]
pub extern "C" fn kapi_hrtimer_cancel(h: usize) { if h != 0 { unsafe { (*(h as *const sched::hrtimer::HrTimer)).cancel(); } } }

#[no_mangle]
pub extern "C" fn kapi_preempt_disable() { sched::preempt_disable() }
#[no_mangle]
pub extern "C" fn kapi_preempt_enable() { sched::preempt_enable() }

/// Fill `buf` from the kernel CSPRNG.
#[no_mangle]
pub extern "C" fn kapi_random_fill(buf: *mut u8, len: usize) {
    if buf.is_null() || len == 0 { return; }
    let s = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    for c in s.chunks_mut(sched::random::CHUNK) { sched::random::fill(c); }
}

/// 0, or -14 for a bad user range, -22 for a null/oversized kernel side. Ioctl (user-thread) context only.
#[no_mangle]
pub extern "C" fn kapi_copy_from_user(dst: *mut u8, uaddr: usize, n: usize) -> i32 {
    if dst.is_null() || n > (1 << 24) { return -22; }
    if unsafe { sched::uaccess::copy_from_user(dst, uaddr, n) } == 0 { 0 } else { -14 }
}
#[no_mangle]
pub extern "C" fn kapi_copy_to_user(uaddr: usize, src: *const u8, n: usize) -> i32 {
    if src.is_null() || n > (1 << 24) { return -22; }
    if unsafe { sched::uaccess::copy_to_user(uaddr, src, n) } == 0 { 0 } else { -14 }
}

/// `index`-th PCI function in bus order (every function). 0 ok, -19 past the end, -22 bad out.
#[no_mangle]
pub extern "C" fn kapi_pci_scan(index: u32, out: *mut KPciDev) -> i32 {
    if out.is_null() { return -22; }
    match drivers::pci::scan().into_iter().nth(index as usize) {
        Some(d) => {
            unsafe {
                out.write(KPciDev {
                    bdf: (d.bus as u32) << 16 | (d.dev as u32) << 8 | d.func as u32,
                    vendor: d.vendor_id, device: d.device_id,
                    class: (d.class as u32) << 16 | (d.subclass as u32) << 8 | d.prog_if as u32,
                    _pad: 0, bars: d.bars,
                });
            }
            0
        }
        None => -19,
    }
}

static MSIX_TABLES: sched::ksync::KSpinLock<Vec<(u32, &'static drivers::irq::MsixTable)>> = sched::ksync::KSpinLock::new(Vec::new());

/// One MSI-X vector (`entry`) of `bdf`; the table is enabled on the first call. Returns the vector or -errno.
#[no_mangle]
pub extern "C" fn kapi_irq_request_msix(owner: KmodId, bdf: u32, entry: u16, handler: KIrqHandler, ctx: usize) -> i64 {
    let known = MSIX_TABLES.lock().iter().find(|t| t.0 == bdf).map(|t| t.1);
    let tab = match known {
        Some(t) => t,
        None => match drivers::irq::MsixTable::enable(super::irq::func_of(bdf)) {
            Ok(t) => {
                let t: &'static drivers::irq::MsixTable = Box::leak(Box::new(t));
                MSIX_TABLES.lock().push((bdf, t));
                t
            }
            Err(e) => return super::irq::irq_errno(e),
        },
    };
    if entry >= tab.entries() { return -22; }
    match tab.request(entry, super::irq::irq_handler(handler), ctx) {
        Ok(h) => {
            let v = h.vector().unwrap_or(0) as u64;
            core::mem::forget(h);
            crate::kmod::registry::note_registration(owner, RegKind::IRQ, v);
            v as i64
        }
        Err(e) => super::irq::irq_errno(e),
    }
}

#[no_mangle]
pub extern "C" fn kapi_fw_root_ready() -> bool { drivers::firmware::root_ready() }

/// Load firmware `path` (not NUL-terminated). Returns a handle (0 on failure) and stores the size and a
/// contiguous kernel mapping in `*size` / `*data`. Task context.
#[no_mangle]
pub extern "C" fn kapi_fw_request(path: *const u8, len: usize, max: usize, size: *mut usize, data: *mut *const u8) -> usize {
    if path.is_null() || len == 0 || len > 1024 || size.is_null() || data.is_null() { return 0; }
    let Ok(p) = core::str::from_utf8(unsafe { core::slice::from_raw_parts(path, len) }) else { return 0 };
    let Ok(mut fw) = drivers::firmware::request_firmware(p, max) else { return 0 };
    let va = if fw.is_empty() { 0 } else { match fw.map() { Some(v) => v, None => return 0 } };
    unsafe { *size = fw.len(); *data = va as *const u8; }
    Box::into_raw(Box::new(fw)) as usize
}
#[no_mangle]
pub extern "C" fn kapi_fw_release(h: usize) {
    if h != 0 { drop(unsafe { Box::from_raw(h as *mut drivers::firmware::Firmware) }); }
}

/// Copy the command-line secret `key` into `buf`. -1 without a network registration by `owner`, -2 absent, -34 too small.
#[no_mangle]
pub extern "C" fn kapi_secret_peek(owner: KmodId, key: *const u8, klen: usize, buf: *mut u8, cap: usize) -> isize {
    if key.is_null() || buf.is_null() || klen == 0 || klen > 64 { return -22; }
    // A network registration, or the Wi-Fi module itself (it reads the legacy credential during probe,
    // before it can register its netif).
    let mut net = crate::kmod::name_str(&crate::kmod::registry::name_of(owner)) == "iwlwifi";
    crate::kmod::registry::registrations(owner, &mut |k, _| if k == RegKind::NETDEV { net = true });
    if !net { return -1; }
    let Ok(k) = core::str::from_utf8(unsafe { core::slice::from_raw_parts(key, klen) }) else { return -22 };
    let Some(v) = drivers::cmdline_secrets::peek(k) else { return -2 };
    if v.len() > cap { return -34; }
    unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len()); }
    v.len() as isize
}

/// `out[0]` = EBUSY refusals, `out[1]` = events pending in the backend.
#[no_mangle]
pub extern "C" fn kapi_ext_event_stats(out: *mut u64) -> i32 {
    if out.is_null() { return -22; }
    let (a, b) = drivers::drm_device_interface::ext_event_stats();
    unsafe { out.write(a); out.add(1).write(b as u64); }
    0
}

/// Append the PRIME/GEM counters line to `buf`; bytes written, 0 when all are zero, -34 when `cap` is too small.
#[no_mangle]
pub extern "C" fn kapi_proc_prime(buf: *mut u8, cap: usize) -> isize {
    if buf.is_null() { return -22; }
    let mut s = String::new();
    if !drivers::drm_device_interface::proc_prime(&mut s) { return 0; }
    if s.len() > cap { return -34; }
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, s.len()); }
    s.len() as isize
}
