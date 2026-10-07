//! `nvrm.testdev=VVVV:DDDD`: drive the RM callbacks this crate implements
//! against a PCI function QEMU can provide, normally the `edu` teaching
//! device (1234:11e8; `-device edu`). It follows the GPU probe path up to,
//! not including, `rm_is_supported_device` (handle, BAR sizing, nv_state_t,
//! rm_init_event_locks), then exercises: config space (incl. a byte write and
//! the extended space), os_map_kernel_space on BAR0, MSI with an ISR that
//! releases an RM semaphore, DMA both ways through nv_alloc_pages /
//! nv_alloc_kernel_mapping (below a 28-bit DMA limit, contiguous + scattered,
//! WB + UC), the lock / wait / delay callbacks, and nv_get_firmware on the
//! staged GSP image. Ends with `[nvrm] testdev: N pass, M fail`.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use drivers::irq::{self, IrqReturn};
use drivers::pci::PciFunction;
use sched::ksync::KSemaphore;

use crate::glue::{self, NvState};
use crate::mem::{NV_MEMORY_CACHED, NV_MEMORY_UNCACHED};
use crate::{nvlog, rm, NV_OK, NV_ERR_TIMEOUT_RETRY};

static PASS: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: core::fmt::Arguments) {
    if ok {
        PASS.fetch_add(1, Ordering::Relaxed);
    } else {
        FAIL.fetch_add(1, Ordering::Relaxed);
    }
    nvlog!("[nvrm] testdev: {} {} {}\n", if ok { "ok  " } else { "FAIL" }, name, detail);
}

macro_rules! chk {
    ($name:expr, $ok:expr) => { check($name, $ok, format_args!("")) };
    ($name:expr, $ok:expr, $($t:tt)*) => { check($name, $ok, format_args!($($t)*)) };
}

// edu registers (QEMU docs/specs/edu.rst)
const EDU_ID: usize = 0x00;
const EDU_LIVE: usize = 0x04;
const EDU_IRQ_STATUS: usize = 0x24;
const EDU_IRQ_RAISE: usize = 0x60;
const EDU_IRQ_ACK: usize = 0x64;
const EDU_DMA_SRC: usize = 0x80;
const EDU_DMA_DST: usize = 0x88;
const EDU_DMA_CNT: usize = 0x90;
const EDU_DMA_CMD: usize = 0x98;
const EDU_BUF: u64 = 0x40000;

static BAR0: AtomicUsize = AtomicUsize::new(0);
static SEM: AtomicUsize = AtomicUsize::new(0);
static IRQS: AtomicU32 = AtomicU32::new(0);

unsafe fn rd32(off: usize) -> u32 {
    core::ptr::read_volatile((BAR0.load(Ordering::Relaxed) + off) as *const u32)
}
unsafe fn wr32(off: usize, v: u32) {
    core::ptr::write_volatile((BAR0.load(Ordering::Relaxed) + off) as *mut u32, v)
}
unsafe fn wr64(off: usize, v: u64) {
    core::ptr::write_volatile((BAR0.load(Ordering::Relaxed) + off) as *mut u64, v)
}

/// Hard-IRQ handler: ack the device, then release the RM semaphore (the
/// os_release_semaphore-from-ISR path rm_isr uses).
extern "C" fn test_isr(_ctx: usize) -> IrqReturn {
    unsafe {
        let st = rd32(EDU_IRQ_STATUS);
        wr32(EDU_IRQ_ACK, st);
        IRQS.fetch_add(1, Ordering::Relaxed);
        crate::sync::os_release_semaphore(SEM.load(Ordering::Relaxed) as *mut c_void);
    }
    IrqReturn::Handled
}

unsafe fn wait_irq(what: &str) -> bool {
    let s = &*(SEM.load(Ordering::Relaxed) as *const KSemaphore);
    let ok = s.down_timeout(2_000_000_000);
    chk!(what, ok, "(irqs so far {})", IRQS.load(Ordering::Relaxed));
    ok
}

pub(crate) fn run(vendor: u16, device: u16) {
    nvlog!("[nvrm] testdev: callback self-test against {:04x}:{:04x}\n", vendor, device);
    let Some(f) = drivers::pci::find_function(vendor, device) else {
        nvlog!("[nvrm] testdev: no {:04x}:{:04x} on the bus (QEMU: -device edu)\n", vendor, device);
        return;
    };
    unsafe { run_on(f) };
    nvlog!("[nvrm] testdev: {} pass, {} fail\n", PASS.load(Ordering::Relaxed), FAIL.load(Ordering::Relaxed));
}

unsafe fn run_on(f: PciFunction) {
    use crate::pcios::*;
    // --- PCI handle + config space
    let (mut v, mut d) = (0u16, 0u16);
    let h = os_pci_init_handle(0, f.bus, f.dev, f.func, &mut v, &mut d);
    chk!("os_pci_init_handle", !h.is_null() && v == f.vendor_id() && d == f.device_id(), "{:04x}:{:04x}", v, d);
    if h.is_null() {
        return;
    }
    let mut id = 0u32;
    let st = os_pci_read_dword(h, 0, &mut id);
    chk!("os_pci_read_dword(0)", st == NV_OK && id == (v as u32 | (d as u32) << 16), "{:#010x}", id);
    let mut line = 0u8;
    os_pci_read_byte(h, 0x3C, &mut line);
    os_pci_write_byte(h, 0x3C, 0x5A);
    let mut back = 0u8;
    os_pci_read_byte(h, 0x3C, &mut back);
    os_pci_write_byte(h, 0x3C, line);
    chk!("os_pci_write_byte/read_byte (0x3C)", back == 0x5A, "wrote 0x5a read {:#x}, restored {:#x}", back, line);
    let mut w = 0u16;
    chk!("os_pci_read_word(2)", os_pci_read_word(h, 2, &mut w) == NV_OK && w == d, "{:#06x}", w);
    let mut ext = 0u32;
    let st = os_pci_read_dword(h, 0x100, &mut ext);
    chk!("os_pci_read_dword(0x100) extended", st == NV_OK, "{:#010x} (ECAM {})", ext, f.has_extended_config());
    chk!("os_pci_read_dword(4096) rejected", os_pci_read_dword(h, 4096, &mut ext) != NV_OK);

    // --- BARs + nv_state_t + RM event locks (the GPU path up to rm_is_supported_device)
    let sizing = f.size_bars();
    let bar0 = sizing.bars.iter().find(|b| b.index == 0 && !b.io && b.size != 0).copied();
    chk!("size_bars BAR0", bar0.is_some(), "{:?}", bar0.map(|b| (b.base, b.size)));
    let Some(bar0) = bar0 else { return };
    let size = glue::lnv_state_size() as usize;
    let nv = alloc::alloc::alloc_zeroed(core::alloc::Layout::from_size_align(size, 16).unwrap()) as *mut NvState;
    glue::lnv_state_init(nv, core::ptr::null_mut(), h, 0, f.bus, f.dev, f.func, v, d, 0, 0, 0);
    glue::lnv_state_set_bar(nv, glue::NV_GPU_BAR_INDEX_REGS, 0, bar0.base, bar0.size);
    let (mut b, mut l) = (0u64, 0u64);
    glue::lnv_state_bar(nv, 0, &mut b, &mut l);
    chk!("nv_state_t BAR0", b == bar0.base && l == bar0.size, "{:#x}+{:#x} ({} bytes of nv_state_t)", b, l, size);
    let ok = rm::rm_init_event_locks(core::ptr::null_mut(), nv);
    chk!("rm_init_event_locks", ok);
    if ok {
        rm::rm_destroy_event_locks(core::ptr::null_mut(), nv);
    }

    // --- os_map_kernel_space on BAR0 (UC)
    f.enable_mem_and_bus_master();
    let va = crate::mem::os_map_kernel_space(bar0.base, 4096, NV_MEMORY_UNCACHED) as usize;
    chk!("os_map_kernel_space(BAR0, UC)", va != 0, "{:#x} -> {:#x}", bar0.base, va);
    if va == 0 {
        return;
    }
    BAR0.store(va, Ordering::Relaxed);
    let ident = rd32(EDU_ID);
    chk!("edu id register", ident & 0xFFFF == 0x00ed, "{:#010x}", ident);
    wr32(EDU_LIVE, 0x1234_5678);
    let live = rd32(EDU_LIVE);
    chk!("edu liveness (MMIO write+read)", live == !0x1234_5678u32, "{:#010x}", live);

    // --- MSI + ISR + semaphore released from IRQ
    let sem = crate::sync::os_alloc_semaphore(0);
    SEM.store(sem as usize, Ordering::Relaxed);
    let msi = irq::request_msi(f, test_isr, 0);
    chk!("request_msi", msi.is_ok(), "vector {:?}", msi.as_ref().ok().and_then(|h| h.vector()));
    if msi.is_ok() {
        wr32(EDU_IRQ_RAISE, 0x1);
        wait_irq("MSI delivered, os_release_semaphore from ISR");
        chk!("os_cond_acquire_semaphore on empty", crate::sync::os_cond_acquire_semaphore(sem) == NV_ERR_TIMEOUT_RETRY);
    }

    // --- DMA RAM -> edu -> RAM through nv_alloc_pages, 28-bit limit
    crate::mem::set_dma_limit_bits(28);
    let mut pte = [0u64; 4];
    let mut a_priv = core::ptr::null_mut();
    let st = crate::mem::nv_alloc_pages(nv as _, 1, 4096, 1, NV_MEMORY_CACHED, 1, 0, -1, pte.as_mut_ptr(), &mut a_priv);
    chk!("nv_alloc_pages contiguous WB below 256 MiB", st == NV_OK && pte[0] < (1 << 28) && pte[0] != 0, "{:#x}", pte[0]);
    let mut b_pte = [0u64; 3];
    let mut b_priv = core::ptr::null_mut();
    let st2 = crate::mem::nv_alloc_pages(nv as _, 3, 4096, 0, NV_MEMORY_UNCACHED, 1, 0, -1, b_pte.as_mut_ptr(), &mut b_priv);
    chk!("nv_alloc_pages scattered UC", st2 == NV_OK && b_pte.iter().all(|&p| p != 0 && p < (1 << 28)), "{:#x} {:#x} {:#x}", b_pte[0], b_pte[1], b_pte[2]);
    if st == NV_OK && st2 == NV_OK && msi.is_ok() {
        let mut ap = core::ptr::null_mut();
        let src = crate::mem::nv_alloc_kernel_mapping(nv as _, a_priv, 0, 0, 4096, &mut ap) as *mut u8;
        chk!("nv_alloc_kernel_mapping WB (HHDM)", !src.is_null() && ap.is_null());
        for i in 0..4096 {
            *src.add(i) = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        wr64(EDU_DMA_SRC, pte[0]);
        wr64(EDU_DMA_DST, EDU_BUF);
        wr64(EDU_DMA_CNT, 4096);
        wr64(EDU_DMA_CMD, 1 | 4);
        wait_irq("DMA RAM->device completion IRQ");
        let mut bp = core::ptr::null_mut();
        let dst = crate::mem::nv_alloc_kernel_mapping(nv as _, b_priv, 1, 16, 4000, &mut bp) as *mut u8;
        chk!("nv_alloc_kernel_mapping UC (vmap, offset 16)", !dst.is_null() && !bp.is_null() && mm::kva::in_window(dst as usize));
        if !dst.is_null() {
            core::ptr::write_bytes(dst.sub(16), 0xEE, 4096);
            wr64(EDU_DMA_SRC, EDU_BUF);
            wr64(EDU_DMA_DST, b_pte[1]);
            wr64(EDU_DMA_CNT, 4096);
            wr64(EDU_DMA_CMD, 1 | 2 | 4);
            wait_irq("DMA device->RAM completion IRQ");
            let base = dst.sub(16);
            let bad = (0..4096).filter(|&i| *base.add(i) != (i as u8).wrapping_mul(7).wrapping_add(3)).count();
            chk!("DMA round trip data (read through UC mapping)", bad == 0, "{} bytes differ", bad);
            crate::mem::nv_free_kernel_mapping(nv as _, b_priv, dst as _, bp);
        }
        crate::mem::nv_free_kernel_mapping(nv as _, a_priv, src as _, ap);
    }
    if st == NV_OK { crate::mem::nv_free_pages(nv as _, 1, 1, 0, a_priv); }
    if st2 == NV_OK { crate::mem::nv_free_pages(nv as _, 3, 0, 1, b_priv); }
    crate::mem::set_dma_limit_bits(32);

    // --- odd-sized contiguous block (tail given back), 2 MiB chunks
    chk!("buddy tail/run decomposition (exact cover, aligned)", crate::mem::pieces_ok());
    let mut big = [0u64; 1];
    let mut big_priv = core::ptr::null_mut();
    let st = crate::mem::nv_alloc_pages(nv as _, 37, 4096, 1, NV_MEMORY_CACHED, 1, 0, -1, big.as_mut_ptr(), &mut big_priv);
    chk!("nv_alloc_pages 37 contiguous pages (64-page block, 27 returned)", st == NV_OK && big[0] % (64 * 4096) == 0, "{:#x}", big[0]);
    if st == NV_OK { crate::mem::nv_free_pages(nv as _, 37, 1, 0, big_priv); }
    let mut huge = alloc::vec![0u64; 1024];
    let mut huge_priv = core::ptr::null_mut();
    let st = crate::mem::nv_alloc_pages(nv as _, 1024, 2 << 20, 0, NV_MEMORY_CACHED, 1, 0, -1, huge.as_mut_ptr(), &mut huge_priv);
    let runs_ok = st == NV_OK && huge[0] % (2 << 20) == 0 && huge[511] == huge[0] + 511 * 4096 && huge[512] % (2 << 20) == 0;
    chk!("nv_alloc_pages 4 MiB in 2 MiB chunks", runs_ok, "{:#x} {:#x}", huge[0], huge[512]);
    if st == NV_OK { crate::mem::nv_free_pages(nv as _, 1024, 0, 0, huge_priv); }
    chk!("all test pages freed", crate::mem::PAGES_LIVE.load(Ordering::Relaxed) == 0 && crate::mem::ALLOCS_LIVE.load(Ordering::Relaxed) == 0,
         "{} pages / {} allocations live", crate::mem::PAGES_LIVE.load(Ordering::Relaxed), crate::mem::ALLOCS_LIVE.load(Ordering::Relaxed));

    // --- locks, wait queue, delays
    let mut m = core::ptr::null_mut();
    crate::sync::os_alloc_mutex(&mut m);
    chk!("os_acquire_mutex", crate::sync::os_acquire_mutex(m) == NV_OK);
    chk!("os_cond_acquire_mutex while held", crate::sync::os_cond_acquire_mutex(m) == NV_ERR_TIMEOUT_RETRY);
    crate::sync::os_release_mutex(m);
    chk!("os_cond_acquire_mutex after release", crate::sync::os_cond_acquire_mutex(m) == NV_OK);
    crate::sync::os_release_mutex(m);
    crate::sync::os_free_mutex(m);
    let rw = crate::sync::os_alloc_rwlock();
    crate::sync::os_acquire_rwlock_read(rw);
    chk!("rwlock: second reader", crate::sync::os_cond_acquire_rwlock_read(rw) == NV_OK);
    chk!("rwlock: writer refused under readers", crate::sync::os_cond_acquire_rwlock_write(rw) == NV_ERR_TIMEOUT_RETRY);
    crate::sync::os_release_rwlock_read(rw);
    crate::sync::os_release_rwlock_read(rw);
    chk!("rwlock: writer after readers", crate::sync::os_cond_acquire_rwlock_write(rw) == NV_OK);
    crate::sync::os_release_rwlock_write(rw);
    crate::sync::os_free_rwlock(rw);
    let mut sl = core::ptr::null_mut();
    crate::sync::os_alloc_spinlock(&mut sl);
    let fl = crate::sync::os_acquire_spinlock(sl);
    chk!("spinlock: sleeping forbidden inside", !sched::irqctx::can_sleep() && crate::sync::os_semaphore_may_sleep() == 0);
    crate::sync::os_release_spinlock(sl, fl);
    chk!("spinlock released: may sleep", crate::sync::os_semaphore_may_sleep() == 1);
    crate::sync::os_free_spinlock(sl);
    let mut wq = core::ptr::null_mut();
    crate::sync::os_alloc_wait_queue(&mut wq);
    let wq_addr = wq as usize;
    let waker = sched::kthread::spawn("nvrm-test", move || {
        sched::kthread::msleep(30);
        crate::sync::os_wake_up(wq_addr as *mut c_void);
    });
    let t0 = sched::monotonic_ns();
    crate::sync::os_wait_uninterruptible(wq);
    let waited = (sched::monotonic_ns() - t0) / 1_000_000;
    chk!("os_wait_uninterruptible woken by os_wake_up from another thread", waker.is_some() && waited >= 20, "{} ms", waited);
    if let Some(k) = waker { k.join(); }
    crate::sync::os_free_wait_queue(wq);
    let t0 = sched::monotonic_ns();
    crate::time::os_delay(20);
    let dl = (sched::monotonic_ns() - t0) / 1000;
    chk!("os_delay(20)", (19_000..200_000).contains(&dl), "{} us", dl);
    let t0 = sched::monotonic_ns();
    crate::time::os_delay_us(300);
    let du = (sched::monotonic_ns() - t0) / 1000;
    chk!("os_delay_us(300)", (290..50_000).contains(&du), "{} us", du);
    let mut rnd = [0u8; 16];
    chk!("os_get_random_bytes", crate::platform::os_get_random_bytes(rnd.as_mut_ptr(), 16) == NV_OK && rnd.iter().any(|&b| b != 0));

    // --- interrupts off, mappings gone
    if let Ok(hd) = msi {
        irq::free_irq(hd);
    }
    crate::mem::os_unmap_kernel_space(va as *mut c_void, 4096);
    crate::sync::os_free_semaphore(sem);

    // --- firmware: the staged GSP image through nv_get_firmware
    let deadline = sched::monotonic_ns() + 120_000_000_000;
    while !drivers::firmware::root_ready() && sched::monotonic_ns() < deadline {
        sched::kthread::msleep(200);
    }
    if !drivers::firmware::root_ready() {
        chk!("root filesystem for firmware", false, "not mounted in 120 s");
        return;
    }
    let pre = crate::fw::preload("gsp_ga10x.bin");
    chk!("firmware preload gsp_ga10x.bin", pre);
    let mut buf: *const c_void = core::ptr::null();
    let mut len = 0u32;
    let hfw = crate::fw::nv_get_firmware(nv as _, 0, 4, &mut buf, &mut len);
    let magic_ok = !buf.is_null() && core::slice::from_raw_parts(buf as *const u8, 4) == b"\x7fELF";
    chk!("nv_get_firmware(GSP, GA10X)", !hfw.is_null() && len == 63_330_032 && magic_ok, "{} bytes at {:p}", len, buf);
    if !hfw.is_null() {
        crate::fw::nv_put_firmware(hfw);
    }
    let hlog = crate::fw::nv_get_firmware(nv as _, 1, 4, &mut buf, &mut len);
    chk!("nv_get_firmware(GSP_LOG) absent -> NULL", hlog.is_null());
}
