//! Cadence macb (Raspberry Pi 5 RP1 Ethernet) as a loadable module: the SAME driver source as the
//! built-in (`drivers/src/macb.rs` + `macb_hw.rs`) with `kenv` and the registry seams pointing at the
//! kmod-kapi shim. The RP1 probe announces `raspberrypi,rp1-gem` to the platform registry; loading the
//! module (autoload by the `of:` alias) registers the driver, which replays that announcement into
//! `macb::probe`. `init` then brings the controller up and registers `eth` through `kapi_netdev_register`.
//! The kexec stop goes through `kapi_dma_quiesce_register` (never a PM hook).
#![no_std]
#![allow(dead_code)]

extern crate alloc;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Kernel-environment seam over the kapi shim.
mod kenv {
    pub use kmod_kapi::sched;
    pub(crate) use crate::mm;
    pub use kmod_kapi::serial_raw;
    use kmod_kapi::{plat, sys_common};

    pub fn ioremap_uc(pa: u64, len: usize) -> Option<usize> { mm::kva::ioremap(pa, len, mm::kva::CacheMode::Uncached) }
    pub fn board_eth_mac() -> Option<[u8; 6]> { plat::mac(plat::board_mac::ETH) }
    pub fn dtb_blob() -> Option<&'static [u8]> { plat::dtb() }
    pub fn rp1_irq_request(n: u32, level: bool, h: kmod_kapi::irq::IrqHandler, ctx: usize) -> bool {
        // `IrqReturn` is `repr(u32)`: the two handler types have the same ABI.
        let h: kmod_abi::KIrqHandler = unsafe { core::mem::transmute(h) };
        plat::rp1_irq_request(n, level, h, ctx).is_ok()
    }

    static QUIESCE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    extern "C" fn quiesce_tramp(_ctx: usize) -> bool {
        let f = QUIESCE.load(core::sync::atomic::Ordering::Acquire);
        if f == 0 { return true; }
        let f: fn() -> bool = unsafe { core::mem::transmute(f) };
        f()
    }
    /// Register the kexec bus-master stop hook (device stage) through `kapi_dma_quiesce_register`.
    pub fn dma_quiesce_register(name: &'static str, f: fn() -> bool) {
        QUIESCE.store(f as usize, core::sync::atomic::Ordering::Release);
        let _ = plat::dma_quiesce_register(name, plat::quiesce_stage::DEVICE, quiesce_tramp, 0);
    }
    #[allow(unused)]
    fn _unused() { let _ = sys_common::can_sleep; }
}

mod mm {
    pub use kmod_kapi::mm::{buddy, phys_to_virt};
    pub mod kva {
        pub use kmod_kapi::mm::kva::{ioremap, CacheMode};
        /// Contiguous alias of `pages` (page addresses).
        pub fn vmap(pages: &[u64], mode: CacheMode) -> Option<usize> {
            kmod_kapi::plat::vmap(pages, match mode { CacheMode::Uncached => 0, CacheMode::WriteCombining => 1, CacheMode::WriteBack => 2 })
        }
    }
    pub mod dma_cache {
        use kmod_kapi::{plat::dma_dir, sys_common};
        pub fn coherent() -> bool { sys_common::dma_coherent() }
        pub fn sync_for_device(va: usize, len: usize) { sys_common::dma_sync(va, len, 0) }
        pub fn sync_for_cpu(va: usize, len: usize) { sys_common::dma_sync(va, len, 1) }
        pub fn clean_range(va: usize, len: usize) { sys_common::dma_sync(va, len, dma_dir::CLEAN) }
        pub fn invalidate_range(va: usize, len: usize) { sys_common::dma_sync(va, len, dma_dir::INVALIDATE) }
    }
}

mod irq {
    pub use kmod_kapi::irq::IrqReturn;
}

mod pci {
    pub use kmod_kapi::pci::PciFunction;
    pub fn find_function(vendor_id: u16, device_id: u16) -> Option<PciFunction> {
        kmod_kapi::pci::find_device(vendor_id, device_id).map(|d| d.function())
    }
}

/// The `drivers::netdev` surface macb touches.
mod netdev {
    pub use kmod_kapi::netdev::{kick, kick_from_irq};
    pub struct NetifOps {
        pub driver: fn(u8) -> &'static str,
        pub send: fn(u8, &[u8]) -> bool,
        pub tx_ready: fn(u8) -> bool,
        pub recv: fn(u8, &mut [u8]) -> Option<usize>,
        pub rx_rearm: fn(u8) -> bool,
        pub rx_irq_driven: fn(u8) -> bool,
        pub mac: fn(u8) -> Option<[u8; 6]>,
        pub link: fn(u8) -> (bool, u32, bool),
        pub link_gen: fn(u8) -> u32,
        pub info: fn(u8, &mut alloc::string::String),
    }
}

/// The `drivers::platform` surface macb touches, over `kapi_plat_register`.
mod platform {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use kmod_abi::KPlatDev;

    #[derive(Clone, Copy, Default)]
    pub struct PlatDev {
        pub base: u64,
        pub size: u64,
        pub irq: u32,
        pub dma_offset: u64,
        pub flags: u32,
        pub regs: [(u64, u64); 4],
        pub irqs: [u32; 4],
        pub nirq: u32,
        pub dt_node: u32,
    }
    pub type ProbeFn = fn(&str, &PlatDev) -> bool;

    static PROBE: AtomicUsize = AtomicUsize::new(0);
    static COMPAT0: AtomicUsize = AtomicUsize::new(0);
    static COMPAT0_LEN: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn tramp(_ctx: usize, dev: *const KPlatDev) -> bool {
        let k = unsafe { &*dev };
        let mut d = PlatDev { dma_offset: k.dma_offset, flags: k.flags, nirq: k.nirq.min(4), dt_node: k.dt_node, ..Default::default() };
        for i in 0..4 { d.regs[i] = (k.reg[i].phys, k.reg[i].len); d.irqs[i] = k.irq[i]; }
        d.base = d.regs[0].0; d.size = d.regs[0].1; d.irq = d.irqs[0];
        let compat = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(COMPAT0.load(Ordering::Relaxed) as *const u8, COMPAT0_LEN.load(Ordering::Relaxed))) };
        let f = PROBE.load(Ordering::Acquire);
        if f == 0 { return false; }
        let f: ProbeFn = unsafe { core::mem::transmute(f) };
        f(compat, &d)
    }

    pub fn register(compats: &'static [&'static str], probe: ProbeFn) {
        PROBE.store(probe as usize, Ordering::Release);
        // A single compatible: every claimed device matches compat_idx 0.
        COMPAT0.store(compats[0].as_ptr() as usize, Ordering::Relaxed);
        COMPAT0_LEN.store(compats[0].len(), Ordering::Relaxed);
        let _ = kmod_kapi::plat::register(compats, tramp, 0);
    }
}

#[path = "../../../drivers/src/macb_hw.rs"]
mod macb_hw;
#[path = "../../../drivers/src/macb.rs"]
mod macb;

kmod_rt::module! {
    name: "macb",
    init: init,
    of: ["raspberrypi,rp1-gem"]
}

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn send(_: usize, b: *const u8, len: usize) -> bool {
    (macb::MACB_OPS.send)(0, unsafe { core::slice::from_raw_parts(b, len) })
}
extern "C" fn tx_ready(_: usize) -> bool { (macb::MACB_OPS.tx_ready)(0) }
extern "C" fn recv(_: usize, b: *mut u8, cap: usize) -> i32 {
    match (macb::MACB_OPS.recv)(0, unsafe { core::slice::from_raw_parts_mut(b, cap) }) {
        Some(n) => n as i32,
        None => -1,
    }
}
extern "C" fn rx_rearm(_: usize) -> bool { (macb::MACB_OPS.rx_rearm)(0) }
extern "C" fn rx_irq_driven(_: usize) -> bool { (macb::MACB_OPS.rx_irq_driven)(0) }
extern "C" fn mac(_: usize, out: *mut u8) -> bool {
    match (macb::MACB_OPS.mac)(0) {
        Some(m) => { unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), out, 6); } true }
        None => false,
    }
}
extern "C" fn link(_: usize, up: *mut bool, mbps: *mut u32, full: *mut bool) {
    let (u, m, f) = (macb::MACB_OPS.link)(0);
    unsafe { *up = u; *mbps = m; *full = f; }
}
extern "C" fn link_gen(_: usize) -> u32 { (macb::MACB_OPS.link_gen)(0) }
extern "C" fn info(_: usize, buf: *mut u8, cap: usize) -> usize {
    let mut s = String::new();
    (macb::MACB_OPS.info)(0, &mut s);
    copy_out(&s, buf, cap)
}
extern "C" fn driver_name(_: usize, buf: *mut u8, cap: usize) -> usize { copy_out("macb", buf, cap) }

static OPS: kmod_kapi::netdev::KNetifOps = kmod_kapi::netdev::KNetifOps {
    send, tx_ready, recv, rx_rearm, rx_irq_driven, mac, link, link_gen, info, driver_name,
};

static DONE: AtomicBool = AtomicBool::new(false);
static _KEEP: AtomicUsize = AtomicUsize::new(0);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    // Registering replays the RP1 announcement into `macb::probe` (rp1_attach); no announcement = nothing attached.
    macb::register();
    macb::init(kmod_kapi::cmdline());
    if !macb::present() { return -19; }
    match kmod_kapi::netdev::register(kmod_kapi::netdev::Kind::Wired, "eth", &OPS, 0) {
        Ok(id) => kmod_rt::kinfo!("registered netif {}", id),
        Err(e) => { kmod_rt::kerr!("netdev_register failed: {}", e); return e; }
    }
    0
}
